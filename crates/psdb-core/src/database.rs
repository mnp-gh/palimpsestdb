//! The entry point: a [`Database`] over one Storage Engine.

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Mutex;

use bson::{Bson, Document};
use psdb_storage::{KeyRange, StorageEngine};

use crate::catalog::CollectionEntry;
use crate::document::{self, Limits};
use crate::error::{Error, Result};
use crate::index::{self, FieldType, IndexDef, IndexInfo};
use crate::keys;
use crate::mvcc;
use crate::transaction::{self, Transaction, WriteSet};
use crate::wal::{self, OsWalFile, Wal, WalFile};

/// An Index build commits the entries for at most this many Documents at a
/// time...
const BUILD_CHUNK_DOCUMENTS: usize = 1024;
/// ...and stops reading a chunk once its Documents reach this many bytes,
/// so neither its memory nor its WAL Record grows with Document size.
const BUILD_CHUNK_BYTES: usize = 16 * 1024 * 1024;

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

    /// The limits Documents and Transactions are checked against.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Log, Apply and publish a Write Set that has passed its conflict
    /// checks, at the next Commit Timestamp.
    pub(crate) fn apply_commit(
        &self,
        commits: &mut CommitState,
        write_set: &WriteSet,
    ) -> Result<u64> {
        let commit_ts = commits.last + 1;
        let Some(wal) = &mut commits.wal else {
            self.engine.write_batch(mvcc::commit_batch(
                commit_ts,
                transaction::writes(write_set),
            ))?;
            commits.last = commit_ts;
            return Ok(commit_ts);
        };
        let record = wal::encode_record(commit_ts, transaction::writes(write_set))?;
        // Once the record may be in the WAL, any failure leaves the outcome
        // unknown, and retrying fsync is unsafe (ADR 0004).
        let outcome = wal.append(&record).map_err(Error::Io).and_then(|()| {
            let batch = mvcc::commit_batch(commit_ts, transaction::writes(write_set));
            Ok(self.engine.write_batch(batch)?)
        });
        if let Err(cause) = outcome {
            commits.poisoned = true;
            return Err(Error::CommitOutcomeUnknown(Box::new(cause)));
        }
        commits.last = commit_ts;
        Ok(commit_ts)
    }

    /// Create an Index on a field and build it from the Documents already in
    /// the Collection (ADR 0010). The field's type must be declared in the
    /// Collection Schema; a unique Index needs a Strict Field.
    ///
    /// The build runs in the foreground, holding the commit lock, so no
    /// other commit happens until it finishes. It commits the entries in
    /// chunks, each an ordinary fsynced commit, so it never holds more than
    /// one chunk in memory; the entries stay unreachable until a final
    /// commit adds the Index to the Catalog. If the build fails, for
    /// example with [`Error::DuplicateValue`], or the Node crashes, the
    /// committed chunks are left behind under an Index ID that is never
    /// reused, until garbage collection exists.
    ///
    /// ```
    /// use bson::doc;
    /// use psdb_core::{Database, FieldType, IndexType};
    /// use psdb_storage_memory::MemoryEngine;
    ///
    /// let db = Database::new(MemoryEngine::new());
    /// db.create_database("shop")?;
    /// db.create_collection("shop", "orders")?;
    /// db.insert("shop", "orders", doc! { "total": 30 })?;
    /// let total = FieldType { index_type: IndexType::Int, strict: false };
    /// db.set_field_type("shop", "orders", "total", total)?;
    /// db.create_index("shop", "orders", "total", false)?;
    /// assert_eq!(db.find("shop", "orders", &doc! { "total": { "$gt": 10 } })?.len(), 1);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn create_index(
        &self,
        database: &str,
        collection: &str,
        path: &str,
        unique: bool,
    ) -> Result<()> {
        let mut commits = self.commits.lock().expect("lock poisoned");
        if commits.poisoned {
            return Err(Error::Poisoned);
        }
        let mut txn = Transaction::new(self, commits.last);
        let (db_id, mut entry, index) = txn.prepare_index(database, collection, path, unique)?;
        self.build_index(&mut commits, db_id, &entry, &index, txn.snapshot_ts())?;
        entry.indexes.push(index);
        txn.write_collection_entry(db_id, &entry)?;
        txn.commit_locked(&mut commits)?;
        Ok(())
    }

    /// Commit an entry in `index` for every Document of the Collection at
    /// `snapshot_ts`, one chunk at a time.
    fn build_index(
        &self,
        commits: &mut CommitState,
        db_id: u32,
        entry: &CollectionEntry,
        index: &IndexDef,
        snapshot_ts: u64,
    ) -> Result<()> {
        let ty = entry.schema[&index.path].index_type;
        let segments = entry.segments(&index.path)?;
        let prefix = keys::collection_prefix(db_id, entry.id);
        let mut range = KeyRange::prefix(&prefix);
        loop {
            let mut page = Vec::new();
            let mut page_bytes = 0;
            let mut more = false;
            mvcc::scan_range_at(&self.engine, &range, snapshot_ts, &mut |key, value| {
                page_bytes += value.len();
                page.push((key.to_vec(), value));
                more = page.len() == BUILD_CHUNK_DOCUMENTS || page_bytes >= BUILD_CHUNK_BYTES;
                if more {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })?;
            let mut chunk = WriteSet::new();
            for (key, bytes) in &page {
                let Some(c) = index::component_of(document::raw(bytes)?, segments, ty)? else {
                    continue;
                };
                let doc_id = &key[prefix.len()..];
                let entry_key = index::entry_key(db_id, index, &c, doc_id);
                if index.unique
                    && !keys::is_null(&c)
                    && (chunk.contains_key(&entry_key)
                        || mvcc::read_at(&self.engine, &entry_key, commits.last)?.is_some())
                {
                    return Err(Error::DuplicateValue(format!(
                        "two documents share a value of {}, so it cannot have a unique index",
                        index.path
                    )));
                }
                chunk.insert(entry_key, Some(doc_id.to_vec()));
            }
            if !chunk.is_empty() {
                self.apply_commit(commits, &chunk)?;
            }
            // `more` means the chunk filled up, not that the Collection ended.
            let (true, Some((last, _))) = (more, page.last()) else {
                return Ok(());
            };
            // Past every Version of the last Document read.
            range.start = [keys::physical_key(last, 0).as_slice(), &[0]].concat();
        }
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

    /// Autocommit [`Transaction::set_field_type`].
    pub fn set_field_type(
        &self,
        database: &str,
        collection: &str,
        path: &str,
        field: FieldType,
    ) -> Result<()> {
        self.run(|t| t.set_field_type(database, collection, path, field))
    }

    /// Autocommit [`Transaction::remove_field_type`].
    pub fn remove_field_type(&self, database: &str, collection: &str, path: &str) -> Result<()> {
        self.run(|t| t.remove_field_type(database, collection, path))
    }

    /// Autocommit [`Transaction::collection_schema`].
    pub fn collection_schema(
        &self,
        database: &str,
        collection: &str,
    ) -> Result<BTreeMap<String, FieldType>> {
        self.run(|t| t.collection_schema(database, collection))
    }

    /// Autocommit [`Transaction::drop_index`].
    pub fn drop_index(&self, database: &str, collection: &str, path: &str) -> Result<()> {
        self.run(|t| t.drop_index(database, collection, path))
    }

    /// Autocommit [`Transaction::list_indexes`].
    pub fn list_indexes(&self, database: &str, collection: &str) -> Result<Vec<IndexInfo>> {
        self.run(|t| t.list_indexes(database, collection))
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
