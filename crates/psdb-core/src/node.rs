//! The entry point: a [`Node`] over one Storage Engine.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use bson::{Bson, Document};
use psdb_storage::StorageEngine;
use uuid::Uuid;

use crate::catalog::CatalogCache;
use crate::collection_path::CollectionPath;
use crate::document::Limits;
use crate::error::{Error, Result};
use crate::field_path::FieldPath;
use crate::index::IndexInfo;
use crate::keys;
use crate::mvcc;
use crate::schema::FieldSchema;
use crate::transaction::{Transaction, WriteSet, writes};
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
    pub(crate) id_generator: IdGenerator,
    /// Holding the lock serialises commits, which keeps them logged and
    /// applied in Commit Timestamp order.
    pub(crate) commit_lock: Mutex<CommitState>,
    /// The newest Visible Commit Timestamp: its writes are Applied to the
    /// engine, so new Snapshots are taken here. A commit may already be in
    /// the WAL with a newer timestamp; it becomes Visible only once Applied.
    ///
    /// Only the commit lock's holder stores it, after the apply. It lives
    /// outside [`CommitState`] because `begin_transaction()` and reads load it without
    /// taking the commit lock.
    pub(crate) latest_visible_commit_ts: AtomicU64,
    /// Set by the commit lock's holder when a commit's outcome is unknown;
    /// nothing more may commit. Outside [`CommitState`] so read-only commits
    /// check it without taking the commit lock.
    pub(crate) poisoned: AtomicBool,
    /// Only the commit lock's holder updates it, after the engine write and
    /// before publishing. Its own lock keeps Catalog reads off the commit lock.
    pub(crate) catalog_cache: CatalogCache,
    /// Active Snapshot timestamps mapped to how many open Transactions read them.
    pub(crate) snapshot_registry: Mutex<BTreeMap<u64, usize>>,
    /// Held by a Garbage Collection pass: a later pass could delete Catalog
    /// Versions an earlier one still reads at its older Horizon. Commits
    /// never take it.
    pub(crate) gc_lock: Mutex<()>,
}

/// Where a Node gets new IDs. Tests inject deterministic sources; the
/// defaults are the OS clock and random number generator.
pub struct IdGenerator {
    /// A new generated Document ID: a UUIDv7 (ADR 0009). IDs from one
    /// source must sort in creation order.
    pub document_id: Box<dyn Fn() -> Uuid + Send + Sync>,
    /// A random candidate for a new Namespace or Collection ID (ADR 0006).
    pub catalog_id: Box<dyn Fn() -> u32 + Send + Sync>,
}

#[allow(
    clippy::disallowed_methods,
    reason = "the OS clock and random number generator behind the IdGenerator seam"
)]
impl Default for IdGenerator {
    fn default() -> Self {
        Self {
            document_id: Box::new(Uuid::now_v7),
            catalog_id: Box::new(|| {
                getrandom::u32().expect("the OS random number generator failed")
            }),
        }
    }
}

/// The state commits serialise on, behind [`Node`]'s commit lock. Only
/// [`Transaction::commit_under_lock`] should change it, so every commit is
/// checked, logged and Applied in one place; `apply_commit` skips the checks.
pub(crate) struct CommitState {
    /// `None` when nothing is durable.
    wal: Option<Wal>,
    /// Collection prefix → Commit Timestamp of the last commit since the
    /// Node opened that wrote a Document there. Index entries do not count.
    /// Entries of dropped Collections stay: a few bytes each (see
    /// .scratch/v1.0-hardening/issues/02-collection-updated-at-growth.md).
    pub(crate) collection_updated_at: HashMap<Vec<u8>, u64>,
}

impl CommitState {
    pub(crate) fn new(wal: Option<Wal>) -> Self {
        Self {
            wal,
            collection_updated_at: HashMap::new(),
        }
    }

    /// Log, Apply and publish a Write Set that has passed its conflict
    /// checks, at the Commit Timestamp after `latest_visible_commit_ts`.
    pub(crate) fn apply_commit<E: StorageEngine>(
        &mut self,
        engine: &E,
        catalog_cache: &CatalogCache,
        latest_visible_commit_ts: &AtomicU64,
        poisoned: &AtomicBool,
        write_set: &WriteSet,
    ) -> Result<u64> {
        // Only the commit lock's holder stores it, so this load sees the newest.
        let commit_ts = latest_visible_commit_ts.load(Ordering::Relaxed) + 1;
        match &mut self.wal {
            None => engine.write_batch(mvcc::commit_batch(commit_ts, writes(write_set)))?,
            Some(wal) => {
                let record = wal::encode_record(commit_ts, writes(write_set))?;
                // Once the record may be in the WAL, any failure leaves the outcome
                // unknown, and retrying fsync is unsafe (ADR 0004).
                let outcome = wal.append(&record).map_err(Error::Io).and_then(|()| {
                    let batch = mvcc::commit_batch(commit_ts, writes(write_set));
                    Ok(engine.write_batch(batch)?)
                });
                if let Err(cause) = outcome {
                    poisoned.store(true, Ordering::Relaxed);
                    return Err(Error::CommitOutcomeUnknown(Box::new(cause)));
                }
            }
        }
        // Before publishing: a Snapshot at `commit_ts` must not find the
        // cached entry this commit replaces.
        self.record_collection_updates(commit_ts, write_set);
        catalog_cache.record(commit_ts, write_set);
        latest_visible_commit_ts.store(commit_ts, Ordering::Release);
        Ok(commit_ts)
    }

    fn record_collection_updates(&mut self, commit_ts: u64, write_set: &WriteSet) {
        for key in write_set.keys() {
            if let Some(prefix) = keys::document_collection_prefix(key) {
                self.collection_updated_at
                    .insert(prefix.to_vec(), commit_ts);
            }
        }
    }
}

impl<E: StorageEngine> Node<E> {
    /// Open over `engine` with default [`Limits`], keeping nothing
    /// durable. An engine that already holds data is reopened where it
    /// left off. Fails with [`Error::Corruption`] if it holds data without
    /// `applied_up_to`.
    pub fn new(engine: E) -> Result<Self> {
        Self::with_limits(engine, Limits::default())
    }

    /// Open over `engine` with custom [`Limits`], keeping nothing
    /// durable. Fails as [`new`](Self::new) does.
    pub fn with_limits(engine: E, limits: Limits) -> Result<Self> {
        Self::recover(engine, limits, IdGenerator::default(), None)
    }

    /// Open a durable Node whose WAL lives in `dir`, creating `dir` and
    /// the WAL if needed, and replay into `engine` the WAL Records it does
    /// not hold yet. Fails with [`Error::Io`] if another Node has `dir` open.
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
        Self::open_with(
            engine,
            OsWalFile::open(dir)?,
            Limits::default(),
            IdGenerator::default(),
        )
    }

    /// Open a durable Node over any [`WalFile`], such as a `SimDisk` file
    /// in tests (feature `testing`), taking new IDs from `id_generator`, and replay
    /// into `engine` the WAL Records above its `applied_up_to` (ADR 0015).
    ///
    /// Fails with [`Error::UnrecognisedWal`], or with [`Error::Corruption`]
    /// for a damaged WAL, an engine that holds data without
    /// `applied_up_to`, or an engine ahead of the WAL, without changing any
    /// WAL Record. A torn write at the WAL's end is truncated, and a WAL
    /// cut off while being created is rewritten.
    pub fn open_with(
        engine: E,
        wal: impl WalFile + 'static,
        limits: Limits,
        id_generator: IdGenerator,
    ) -> Result<Self> {
        let wal = Wal::open(Box::new(wal))?;
        Self::recover(engine, limits, id_generator, Some(wal))
    }

    /// Bring `engine` level with the WAL, if any, by replaying the Records
    /// above its `applied_up_to`, then fill the Catalog cache from it.
    fn recover(
        engine: E,
        limits: Limits,
        id_generator: IdGenerator,
        wal: Option<(Wal, Vec<wal::Record>)>,
    ) -> Result<Self> {
        let mut last = mvcc::applied_up_to(&engine)?;
        let wal = match wal {
            None => None,
            Some((wal, records)) => {
                let wal_last = records.last().map_or(0, |r| r.commit_ts);
                if last > wal_last {
                    return Err(Error::Corruption(format!(
                        "the storage engine holds commits up to {last}, past the WAL's last Record {wal_last}"
                    )));
                }
                for record in records.iter().filter(|r| r.commit_ts > last) {
                    engine.write_batch(mvcc::commit_batch(
                        record.commit_ts,
                        writes(&record.write_set),
                    ))?;
                }
                last = wal_last;
                Some(wal)
            }
        };
        let catalog_cache = CatalogCache::load(&engine)?;
        let node = Self::from_parts(engine, limits, id_generator, last, wal, catalog_cache);
        node.drop_building_indexes()?;
        Ok(node)
    }

    fn from_parts(
        engine: E,
        limits: Limits,
        id_generator: IdGenerator,
        last: u64,
        wal: Option<Wal>,
        catalog_cache: CatalogCache,
    ) -> Self {
        Self {
            engine,
            limits,
            id_generator,
            commit_lock: Mutex::new(CommitState::new(wal)),
            latest_visible_commit_ts: AtomicU64::new(last),
            poisoned: AtomicBool::new(false),
            catalog_cache,
            snapshot_registry: Mutex::new(BTreeMap::new()),
            gc_lock: Mutex::new(()),
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
    pub fn begin_transaction(&self) -> Transaction<'_, E> {
        assert!(!self.commit_lock.is_poisoned(), "commit lock poisoned");
        Transaction::new(self)
    }

    /// Run `work` in a new Transaction and commit it if `work` succeeds.
    /// If `work` fails, the Transaction is aborted and its error returned.
    pub fn run<T>(&self, work: impl FnOnce(&mut Transaction<'_, E>) -> Result<T>) -> Result<T> {
        let mut txn = self.begin_transaction();
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
    #[expect(
        clippy::redundant_closure_for_method_calls,
        reason = "`Transaction::list_namespaces` is not general over the Transaction lifetime"
    )]
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
        self.run(|t| t.find(collection, filter)?.collect())
    }

    /// Induces a panic while holding the commit lock to poison it for testing.
    #[cfg(any(test, feature = "testing"))]
    pub fn poison_commit_lock_for_test(&self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = self.commit_lock.lock();
            panic!("test induced panic to poison commit lock");
        }));
    }
}
