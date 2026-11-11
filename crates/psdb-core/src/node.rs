//! The entry point: a [`Node`] over one Storage Engine.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use bson::{Bson, Document};
use psdb_storage::StorageEngine;

use crate::collection_path::CollectionPath;
use crate::document::Limits;
use crate::error::{Error, Result};
use crate::field_path::FieldPath;
use crate::index::IndexInfo;
use crate::mvcc;
use crate::schema::FieldSchema;
use crate::transaction::{self, CommitState, Transaction};
use crate::wal::{OsWalFile, Wal, WalFile};

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
/// let node = Node::new(MemoryEngine::new())?;
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
    pub(crate) commit_lock: Mutex<CommitState>,
    /// The newest Visible Commit Timestamp: its writes are Applied to the
    /// engine, so new Snapshots are taken here. A commit may already be in
    /// the WAL with a newer timestamp; it becomes Visible only once Applied.
    ///
    /// Only the commit lock's holder stores it, after the apply. It lives
    /// outside [`CommitState`] because `begin()` and reads load it without
    /// taking the commit lock.
    pub(crate) latest_visible_commit_ts: AtomicU64,
    /// Active Snapshot timestamps mapped to how many open Transactions read them.
    pub(crate) snapshot_registry: Mutex<BTreeMap<u64, usize>>,
}

impl<E: StorageEngine> Node<E> {
    /// Open over an empty engine with default [`Limits`], keeping nothing
    /// durable. Fails with [`Error::EngineNotEmpty`] if the engine holds
    /// data: its Versions would clash with the Commit Timestamps this Node
    /// gives out, which start again from the beginning.
    pub fn new(engine: E) -> Result<Self> {
        Self::with_limits(engine, Limits::default())
    }

    /// Open over an empty engine with custom [`Limits`], keeping nothing
    /// durable. Fails as [`new`](Self::new) does.
    pub fn with_limits(engine: E, limits: Limits) -> Result<Self> {
        if mvcc::any_version_under(&engine, &[])? {
            return Err(Error::EngineNotEmpty);
        }
        Ok(Self::from_parts(engine, limits, 0, None))
    }

    /// Open a durable Node whose WAL lives in `dir`, creating `dir` and
    /// the WAL if needed, and replay the WAL into `engine`, which must be
    /// empty. Fails with [`Error::Io`] if another Node has `dir` open.
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

    /// Open a durable Node over any [`WalFile`], such as a `SimDisk` file
    /// in tests (feature `testing`), and replay it into
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
            commit_lock: Mutex::new(CommitState::new(wal)),
            latest_visible_commit_ts: AtomicU64::new(last),
            snapshot_registry: Mutex::new(BTreeMap::new()),
        }
    }

    /// The limits Documents and Transactions are checked against.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The newest Commit Timestamp Applied to the engine.
    pub(crate) fn latest_visible_commit_ts(&self) -> u64 {
        self.latest_visible_commit_ts.load(Ordering::Acquire)
    }

    /// The oldest Snapshot any active Transaction reads, or the newest Visible
    /// commit when no Transaction is active (see `GLOSSARY.md`).
    pub fn horizon(&self) -> u64 {
        let reg = self.snapshot_registry.lock().expect("registry poisoned");
        reg.keys()
            .next()
            .copied()
            .unwrap_or_else(|| self.latest_visible_commit_ts())
    }

    pub(crate) fn register_snapshot(&self) -> u64 {
        let mut reg = self.snapshot_registry.lock().expect("registry poisoned");
        let ts = self.latest_visible_commit_ts();
        *reg.entry(ts).or_default() += 1;
        ts
    }

    pub(crate) fn register_snapshot_at(&self, ts: u64) {
        let mut reg = self.snapshot_registry.lock().expect("registry poisoned");
        *reg.entry(ts).or_default() += 1;
    }

    pub(crate) fn unregister_snapshot(&self, snapshot_ts: u64) {
        let mut reg = self.snapshot_registry.lock().expect("registry poisoned");
        if let std::collections::btree_map::Entry::Occupied(mut entry) = reg.entry(snapshot_ts) {
            if *entry.get() <= 1 {
                entry.remove();
            } else {
                *entry.get_mut() -= 1;
            }
        }
    }

    /// Begin a Transaction with a Snapshot of everything committed so far.
    pub fn begin(&self) -> Transaction<'_, E> {
        if self.commit_lock.is_poisoned() {
            panic!("commit lock poisoned");
        }
        let snapshot_ts = self.register_snapshot();
        Transaction::from_registered(self, snapshot_ts)
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

    /// Induces a panic while holding the commit lock to poison it for testing.
    #[cfg(any(test, feature = "testing"))]
    pub fn poison_commit_lock_for_test(&self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = self.commit_lock.lock().unwrap();
            panic!("test induced panic to poison commit lock");
        }));
    }
}
