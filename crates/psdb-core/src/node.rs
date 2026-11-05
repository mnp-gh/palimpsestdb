//! The entry point: a [`Node`] over one Storage Engine.

use std::path::Path;
use std::sync::Mutex;

use bson::{Bson, Document};
use psdb_storage::StorageEngine;

use crate::document::Limits;
use crate::error::{Error, Result};
use crate::mvcc;
use crate::transaction::{self, Transaction};
use crate::wal::{OsWalFile, Wal, WalFile};

/// An embedded PalimpsestDB instance over the Storage Engine `E`.
///
/// Every Namespace and Collection lives in the one engine
/// (ADR 0005). [`open`](Self::open) makes it durable with a WAL (ADR 0004);
/// [`new`](Self::new) keeps nothing durable.
///
/// ```
/// use bson::doc;
/// use psdb_core::Node;
/// use psdb_storage_memory::MemoryEngine;
///
/// let node = Node::new(MemoryEngine::new());
/// node.create_namespace("shop")?;
/// node.create_collection("shop", "orders")?;
/// let id = node.insert("shop", "orders", doc! { "item": "book" })?;
/// assert_eq!(node.get("shop", "orders", &id)?.unwrap().get_str("item")?, "book");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct Node<E: StorageEngine> {
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

impl<E: StorageEngine> Node<E> {
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

    /// Open a durable Node whose WAL lives in `dir`, creating `dir` and
    /// the WAL if needed, and replay the WAL into `engine`, which must be
    /// empty.
    ///
    /// ```
    /// use bson::doc;
    /// use psdb_core::Node;
    /// use psdb_storage_memory::MemoryEngine;
    ///
    /// # let dir = std::env::temp_dir().join(format!("psdb-doc-{}", std::process::id()));
    /// # let _ = std::fs::remove_dir_all(&dir);
    /// let node = Node::open(MemoryEngine::new(), &dir)?;
    /// node.create_namespace("shop")?;
    /// drop(node);
    ///
    /// let node = Node::open(MemoryEngine::new(), &dir)?;
    /// assert_eq!(node.list_namespaces()?, ["shop"]);
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn open(engine: E, dir: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(engine, OsWalFile::open(dir)?, Limits::default())
    }

    /// Open a durable Node over any [`WalFile`], such as a
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
            engine.write_batch(mvcc::commit_batch(
                record.commit_ts,
                transaction::writes(&record.write_set),
            ))?;
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

    /// Autocommit [`Transaction::create_namespace`].
    pub fn create_namespace(&self, name: &str) -> Result<()> {
        self.run(|t| t.create_namespace(name))
    }

    /// Autocommit [`Transaction::drop_namespace`].
    pub fn drop_namespace(&self, name: &str) -> Result<()> {
        self.run(|t| t.drop_namespace(name))
    }

    /// Autocommit [`Transaction::list_namespaces`].
    pub fn list_namespaces(&self) -> Result<Vec<String>> {
        self.run(|t| t.list_namespaces())
    }

    /// Autocommit [`Transaction::create_collection`].
    pub fn create_collection(&self, namespace: &str, name: &str) -> Result<()> {
        self.run(|t| t.create_collection(namespace, name))
    }

    /// Autocommit [`Transaction::drop_collection`].
    pub fn drop_collection(&self, namespace: &str, name: &str) -> Result<()> {
        self.run(|t: &mut Transaction<'_, E>| t.drop_collection(namespace, name))
    }

    /// Autocommit [`Transaction::list_collections`].
    pub fn list_collections(&self, namespace: &str) -> Result<Vec<String>> {
        self.run(|t| t.list_collections(namespace))
    }

    /// Autocommit [`Transaction::insert`].
    pub fn insert(&self, namespace: &str, collection: &str, doc: Document) -> Result<Bson> {
        self.run(|t| t.insert(namespace, collection, doc))
    }

    /// Autocommit [`Transaction::get`].
    pub fn get(&self, namespace: &str, collection: &str, id: &Bson) -> Result<Option<Document>> {
        self.run(|t| t.get(namespace, collection, id))
    }

    /// Autocommit [`Transaction::replace`].
    pub fn replace(
        &self,
        namespace: &str,
        collection: &str,
        id: &Bson,
        doc: Document,
    ) -> Result<bool> {
        self.run(|t| t.replace(namespace, collection, id, doc))
    }

    /// Autocommit [`Transaction::delete`].
    pub fn delete(&self, namespace: &str, collection: &str, id: &Bson) -> Result<bool> {
        self.run(|t| t.delete(namespace, collection, id))
    }

    /// Autocommit [`Transaction::find`].
    pub fn find(
        &self,
        namespace: &str,
        collection: &str,
        filter: &Document,
    ) -> Result<Vec<Document>> {
        self.run(|t| t.find(namespace, collection, filter))
    }
}
