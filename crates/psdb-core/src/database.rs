//! The entry point: a [`Database`] over one Storage Engine.

use std::path::Path;
use std::sync::Mutex;

use bson::{Bson, Document};
use psdb_storage::StorageEngine;

use crate::document::Limits;
use crate::error::{Error, Result};
use crate::mvcc;
use crate::transaction::Transaction;
use crate::wal::{OsWalFile, Wal, WalFile};

/// An embedded PalimpsestDB instance over the Storage Engine `E`.
///
/// Every Database (namespace) and Collection lives in the one engine
/// (ADR 0005). [`open`](Self::open) makes it durable with a WAL (ADR 0004);
/// [`new`](Self::new) keeps nothing durable.
///
/// ```
/// use bson::doc;
/// use psdb_core::Database;
/// use psdb_storage_memory::MemoryEngine;
///
/// let db = Database::new(MemoryEngine::new());
/// db.create_database("shop")?;
/// db.create_collection("shop", "orders")?;
/// let id = db.insert("shop", "orders", doc! { "item": "book" })?;
/// assert_eq!(db.get("shop", "orders", &id)?.unwrap().get_str("item")?, "book");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct Database<E: StorageEngine> {
    pub(crate) engine: E,
    pub(crate) limits: Limits,
    /// Holding the lock serialises commits, which keeps them logged and
    /// applied in Commit Timestamp order.
    pub(crate) commits: Mutex<CommitState>,
}

/// The state commits serialise on.
pub(crate) struct CommitState {
    /// The newest Commit Timestamp; new Snapshots are taken here.
    pub(crate) last: u64,
    /// `None` when nothing is durable.
    pub(crate) wal: Option<Wal>,
    /// Set when a commit's outcome is unknown; nothing more may commit.
    pub(crate) poisoned: bool,
}

impl<E: StorageEngine> Database<E> {
    /// Open over an empty engine with default [`Limits`], keeping nothing
    /// durable.
    pub fn new(engine: E) -> Self {
        Self::with_limits(engine, Limits::default())
    }

    /// Open over an empty engine with custom [`Limits`], keeping nothing
    /// durable.
    pub fn with_limits(engine: E, limits: Limits) -> Self {
        Self::from_parts(engine, limits, 0, None)
    }

    /// Open a durable Database whose WAL lives in `dir`, creating `dir` and
    /// the WAL if needed, and replay the WAL into `engine`, which must be
    /// empty.
    ///
    /// ```
    /// use bson::doc;
    /// use psdb_core::Database;
    /// use psdb_storage_memory::MemoryEngine;
    ///
    /// # let dir = std::env::temp_dir().join(format!("psdb-doc-{}", std::process::id()));
    /// # let _ = std::fs::remove_dir_all(&dir);
    /// let db = Database::open(MemoryEngine::new(), &dir)?;
    /// db.create_database("shop")?;
    /// drop(db);
    ///
    /// let db = Database::open(MemoryEngine::new(), &dir)?;
    /// assert_eq!(db.list_databases()?, ["shop"]);
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn open(engine: E, dir: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(engine, OsWalFile::open(dir)?, Limits::default())
    }

    /// Open a durable Database over any [`WalFile`], such as a
    /// [`SimDisk`](crate::wal::SimDisk) file in tests, and replay it into
    /// `engine`, which must be empty.
    ///
    /// Fails with [`Error::EngineNotEmpty`], [`Error::UnrecognisedWal`] or
    /// [`Error::Corruption`] without changing the WAL. On success, a torn
    /// write at the WAL's end has been truncated, and a WAL cut off while
    /// being created has been rewritten.
    pub fn open_with(engine: E, wal: impl WalFile + 'static, limits: Limits) -> Result<Self> {
        if mvcc::any_version_under(&engine, &[])? {
            return Err(Error::EngineNotEmpty);
        }
        let (wal, records) = Wal::open(Box::new(wal))?;
        let mut last = 0;
        for record in &records {
            engine.write_batch(mvcc::commit_batch(record.commit_ts, record.writes()))?;
            last = record.commit_ts;
        }
        Ok(Self::from_parts(engine, limits, last, Some(wal)))
    }

    fn from_parts(engine: E, limits: Limits, last: u64, wal: Option<Wal>) -> Self {
        Self {
            engine,
            limits,
            commits: Mutex::new(CommitState {
                last,
                wal,
                poisoned: false,
            }),
        }
    }

    /// The limits Documents are checked against.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Begin a Transaction with a Snapshot of everything committed so far.
    pub fn begin(&self) -> Transaction<'_, E> {
        let snapshot_ts = self.commits.lock().expect("lock poisoned").last;
        Transaction::new(self, snapshot_ts)
    }

    /// Run `work` in a new Transaction and commit it if `work` succeeds.
    /// If `work` fails, the Transaction is aborted and its error returned.
    pub fn run<T>(&self, work: impl FnOnce(&mut Transaction<'_, E>) -> Result<T>) -> Result<T> {
        let mut txn = self.begin();
        let out = work(&mut txn)?;
        txn.commit()?;
        Ok(out)
    }

    /// Autocommit [`Transaction::create_database`].
    pub fn create_database(&self, name: &str) -> Result<()> {
        self.run(|t| t.create_database(name))
    }

    /// Autocommit [`Transaction::drop_database`].
    pub fn drop_database(&self, name: &str) -> Result<()> {
        self.run(|t| t.drop_database(name))
    }

    /// Autocommit [`Transaction::list_databases`].
    pub fn list_databases(&self) -> Result<Vec<String>> {
        self.run(|t| t.list_databases())
    }

    /// Autocommit [`Transaction::create_collection`].
    pub fn create_collection(&self, database: &str, name: &str) -> Result<()> {
        self.run(|t| t.create_collection(database, name))
    }

    /// Autocommit [`Transaction::drop_collection`].
    pub fn drop_collection(&self, database: &str, name: &str) -> Result<()> {
        self.run(|t: &mut Transaction<'_, E>| t.drop_collection(database, name))
    }

    /// Autocommit [`Transaction::list_collections`].
    pub fn list_collections(&self, database: &str) -> Result<Vec<String>> {
        self.run(|t| t.list_collections(database))
    }

    /// Autocommit [`Transaction::insert`].
    pub fn insert(&self, database: &str, collection: &str, doc: Document) -> Result<Bson> {
        self.run(|t| t.insert(database, collection, doc))
    }

    /// Autocommit [`Transaction::get`].
    pub fn get(&self, database: &str, collection: &str, id: &Bson) -> Result<Option<Document>> {
        self.run(|t| t.get(database, collection, id))
    }

    /// Autocommit [`Transaction::replace`].
    pub fn replace(
        &self,
        database: &str,
        collection: &str,
        id: &Bson,
        doc: Document,
    ) -> Result<bool> {
        self.run(|t| t.replace(database, collection, id, doc))
    }

    /// Autocommit [`Transaction::delete`].
    pub fn delete(&self, database: &str, collection: &str, id: &Bson) -> Result<bool> {
        self.run(|t| t.delete(database, collection, id))
    }

    /// Autocommit [`Transaction::find`].
    pub fn find(
        &self,
        database: &str,
        collection: &str,
        filter: &Document,
    ) -> Result<Vec<Document>> {
        self.run(|t| t.find(database, collection, filter))
    }
}
