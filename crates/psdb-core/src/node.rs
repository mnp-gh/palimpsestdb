//! The entry point: a [`Node`] over one Storage Engine.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use bson::{Bson, Document};
use psdb_storage::StorageEngine;

use crate::collection_path::CollectionPath;
use crate::document::Limits;
use crate::error::{Error, Result};
use crate::field_path::FieldPath;
use crate::index::IndexInfo;
use crate::mvcc;
use crate::schema::FieldSchema;
use crate::transaction::{self, Transaction, WriteSet};
use crate::wal::{self, OsWalFile, Wal, WalFile};

/// An embedded PalimpsestDB Node over the Storage Engine `E`.
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
/// let orders = "shop.orders".parse()?;
/// node.create_collection(&orders)?;
/// let id = node.insert(&orders, doc! { "item": "book" })?;
/// assert_eq!(node.get(&orders, &id)?.unwrap().get_str("item")?, "book");
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
    pub fn create_collection(&self, collection: &CollectionPath) -> Result<()> {
        self.run(|t| t.create_collection(collection))
    }

    /// Autocommit [`Transaction::drop_collection`].
    pub fn drop_collection(&self, collection: &CollectionPath) -> Result<()> {
        self.run(|t| t.drop_collection(collection))
    }

    /// Autocommit [`Transaction::list_collections`].
    pub fn list_collections(&self, namespace: &str) -> Result<Vec<String>> {
        self.run(|t| t.list_collections(namespace))
    }

    /// Autocommit [`Transaction::insert`].
    pub fn insert(&self, collection: &CollectionPath, doc: Document) -> Result<Bson> {
        self.run(|t| t.insert(collection, doc))
    }

    /// Autocommit [`Transaction::get`].
    pub fn get(&self, collection: &CollectionPath, id: &Bson) -> Result<Option<Document>> {
        self.run(|t| t.get(collection, id))
    }

    /// Autocommit [`Transaction::replace`].
    pub fn replace(&self, collection: &CollectionPath, id: &Bson, doc: Document) -> Result<bool> {
        self.run(|t| t.replace(collection, id, doc))
    }

    /// Autocommit [`Transaction::delete`].
    pub fn delete(&self, collection: &CollectionPath, id: &Bson) -> Result<bool> {
        self.run(|t| t.delete(collection, id))
    }

    /// Autocommit [`Transaction::set_field_type`].
    pub fn set_field_type(
        &self,
        collection: &CollectionPath,
        path: &FieldPath,
        field: FieldSchema,
    ) -> Result<()> {
        self.run(|t| t.set_field_type(collection, path, field))
    }

    /// Autocommit [`Transaction::remove_field_type`].
    pub fn remove_field_type(&self, collection: &CollectionPath, path: &FieldPath) -> Result<()> {
        self.run(|t| t.remove_field_type(collection, path))
    }

    /// Autocommit [`Transaction::collection_schema`].
    pub fn collection_schema(
        &self,
        collection: &CollectionPath,
    ) -> Result<BTreeMap<FieldPath, FieldSchema>> {
        self.run(|t| t.collection_schema(collection))
    }

    /// Autocommit [`Transaction::drop_index`].
    pub fn drop_index(&self, collection: &CollectionPath, path: &FieldPath) -> Result<()> {
        self.run(|t| t.drop_index(collection, path))
    }

    /// Autocommit [`Transaction::list_indexes`].
    pub fn list_indexes(&self, collection: &CollectionPath) -> Result<Vec<IndexInfo>> {
        self.run(|t| t.list_indexes(collection))
    }

    /// Autocommit [`Transaction::find`].
    pub fn find(&self, collection: &CollectionPath, filter: &Document) -> Result<Vec<Document>> {
        self.run(|t| t.find(collection, filter))
    }
}
