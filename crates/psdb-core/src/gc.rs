//! Garbage Collection of old Versions (ADR 0012).

use std::collections::{BTreeSet, HashSet};
use std::ops::ControlFlow;
use std::sync::MutexGuard;

use psdb_storage::{KeyRange, StorageEngine, WriteBatch};

use crate::catalog;
use crate::error::Result;
use crate::keys;
use crate::mvcc;
use crate::node::Node;

/// A batch closes at the first logical key after visiting this many
/// physical keys, so a key's Versions are never split across batches and a
/// step holds the engine's read lock for a bounded scan, however little it
/// deletes.
const BATCH_KEYS: usize = 1024;

/// What one [`Node::collect_garbage`] pass removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcStats {
    /// Versions deleted, Tombstones included.
    pub versions_removed: u64,
    /// Tombstones deleted, because each was its key's newest Version at the
    /// Horizon.
    pub tombstones_removed: u64,
    /// Dropped Collections whose Documents or Index entries were deleted.
    pub collections_reclaimed: u64,
    /// Dropped Indexes, in Collections still there, whose entries were
    /// deleted.
    pub indexes_reclaimed: u64,
}

impl<E: StorageEngine> Node<E> {
    /// Remove every Version no Snapshot at or after the Horizon can read:
    /// for each key, the Versions older than its newest one at the Horizon,
    /// and that one too if it is a Tombstone. Deletes straight from the
    /// engine in batches, without the commit lock and without logging, so
    /// Recovery may bring removed Versions back for the next pass to remove.
    ///
    /// Deleting the Tombstone of a Namespace's, Collection's or Index's
    /// Catalog entry makes its ID dead: every key under it is deleted,
    /// whatever its timestamp, since IDs are never reused. So is every
    /// Version of a dead Namespace's Collection entries and a dead
    /// Collection's Index Catalog entries. ID keys stay, so IDs are never
    /// reused. The Catalog sorts before the data, so dead IDs
    /// are known before the scan reaches it.
    ///
    /// One pass runs at a time; a second call waits for the first.
    ///
    /// # Errors
    ///
    /// Only [the errors every operation can return](crate::Error#errors-every-operation-can-return).
    pub fn collect_garbage(&self) -> Result<GcStats> {
        let mut pass = self.start_gc();
        loop {
            if let Some(stats) = pass.step()? {
                return Ok(stats);
            }
        }
    }

    /// Start a pass of [`collect_garbage`](Self::collect_garbage), for the
    /// caller to [`step`](GcPass::step). Waits for any other pass to end.
    #[cfg(any(test, feature = "testing"))]
    pub fn start_garbage_collection(&self) -> GcPass<'_, E> {
        self.start_gc()
    }

    fn start_gc(&self) -> GcPass<'_, E> {
        let one_pass = self.gc_lock.lock().expect("gc lock poisoned");
        GcPass {
            node: self,
            _one_pass: one_pass,
            horizon: self.horizon(),
            stats: GcStats::default(),
            // Unversioned state sorts below the Namespaces, so it is skipped.
            range: KeyRange {
                start: keys::namespaces_prefix(),
                end: None,
            },
            batch_keys: BATCH_KEYS,
            current: Vec::new(),
            seen_newest: false,
            dead_id_follows: false,
            dead_namespaces: HashSet::new(),
            dead_collections: HashSet::new(),
            reclaimed_collections: HashSet::new(),
            dead_indexes: HashSet::new(),
            reclaimed_indexes: HashSet::new(),
            uncached: BTreeSet::new(),
            held_back: Vec::new(),
            done: false,
        }
    }
}

/// A Garbage Collection pass in progress, holding the GC lock; see
/// [`Node::collect_garbage`].
#[cfg_attr(
    not(any(test, feature = "testing")),
    expect(unreachable_pub, reason = "exported only for tests (lib.rs)")
)]
pub struct GcPass<'node, E: StorageEngine> {
    node: &'node Node<E>,
    _one_pass: MutexGuard<'node, ()>,
    horizon: u64,
    stats: GcStats,
    /// The keys not yet visited.
    range: KeyRange,
    /// [`BATCH_KEYS`], unless a test sets it.
    batch_keys: usize,
    /// The logical key being visited, and whether its newest Version at
    /// the Horizon was already seen.
    current: Vec<u8>,
    seen_newest: bool,
    /// Set on deleting a Namespace or Collection entry's Tombstone: the
    /// key's next Version holds the dropped ID.
    dead_id_follows: bool,
    /// IDs of the dropped Namespaces found; their Collections are dead too.
    dead_namespaces: HashSet<u32>,
    /// (Namespace ID, Collection ID) of each dropped Collection found.
    dead_collections: HashSet<(u32, u32)>,
    reclaimed_collections: HashSet<(u32, u32)>,
    /// (Namespace ID, Collection ID, Index ID) of each dropped Index found.
    dead_indexes: HashSet<(u32, u32, u32)>,
    reclaimed_indexes: HashSet<(u32, u32, u32)>,
    /// Catalog entries whose every Version is deleted, to drop from the cache.
    uncached: BTreeSet<Vec<u8>>,
    /// Catalog keys to delete once the data is gone, so a crash never
    /// loses the Tombstone that marks data dead.
    held_back: Vec<Vec<u8>>,
    /// Set once the pass has returned its stats.
    done: bool,
}

#[cfg_attr(
    not(any(test, feature = "testing")),
    expect(unreachable_pub, reason = "exported only for tests (lib.rs)")
)]
impl<E: StorageEngine> GcPass<'_, E> {
    /// Close each batch at the first logical key after visiting `n`
    /// physical keys, so tests can commit between small batches.
    #[cfg(any(test, feature = "testing"))]
    pub fn with_batch_keys(mut self, n: usize) -> Self {
        self.batch_keys = n;
        self
    }

    /// Delete the next batch of keys; once every key is visited, finish
    /// the pass and return what it removed.
    ///
    /// Panics if the pass already finished.
    ///
    /// # Errors
    ///
    /// Only [the errors every operation can return](crate::Error#errors-every-operation-can-return).
    pub fn step(&mut self) -> Result<Option<GcStats>> {
        assert!(!self.done, "stepped a finished Garbage Collection pass");
        let node = self.node;
        // A copy, so `visit` can borrow `self` mutably during the scan.
        let range = self.range.clone();
        let mut batch = WriteBatch::new();
        let mut visited = 0;
        let mut next_start = None;
        let mut failure = None;
        node.engine.scan(&range, &mut |key, value| {
            visited += 1;
            match self.visit(key, value, visited, &mut batch) {
                Ok(ControlFlow::Continue(())) => ControlFlow::Continue(()),
                Ok(ControlFlow::Break(())) => {
                    next_start = Some(key.to_vec());
                    ControlFlow::Break(())
                }
                Err(e) => {
                    failure = Some(e);
                    ControlFlow::Break(())
                }
            }
        })?;
        if let Some(e) = failure {
            return Err(e);
        }
        if !batch.ops().is_empty() {
            node.engine.write_batch(batch)?;
        }
        if let Some(start) = next_start {
            self.range.start = start;
            return Ok(None);
        }
        self.finish().map(Some)
    }

    /// Visit the physical `key`, the `visited`th of this step: queue it in
    /// `batch` if it is garbage. Breaks to close the batch before `key`.
    fn visit(
        &mut self,
        key: &[u8],
        value: &[u8],
        visited: usize,
        batch: &mut WriteBatch,
    ) -> Result<ControlFlow<()>> {
        let (logical, ts) = keys::split_physical(key)?;
        if logical != self.current.as_slice() {
            // Close the batch only between keys: deleting a Tombstone
            // without the Versions under it would show them to readers.
            if visited > self.batch_keys {
                return Ok(ControlFlow::Break(()));
            }
            self.current = logical.to_vec();
            self.seen_newest = false;
            self.dead_id_follows = false;
        }
        if self.reclaim(logical) {
            self.stats.versions_removed += 1;
            self.stats.tombstones_removed += u64::from(mvcc::is_tombstone(value));
            self.delete(key, logical, batch);
            return Ok(ControlFlow::Continue(()));
        }
        if ts > self.horizon {
            return Ok(ControlFlow::Continue(()));
        }
        if !self.seen_newest {
            self.seen_newest = true;
            if !mvcc::is_tombstone(value) {
                return Ok(ControlFlow::Continue(()));
            }
            self.stats.tombstones_removed += 1;
        }
        self.stats.versions_removed += 1;
        self.note_drop(logical, value)?;
        self.delete(key, logical, batch);
        Ok(ControlFlow::Continue(()))
    }

    /// Whether `logical` belongs to a dropped Namespace, Collection or
    /// Index found so far, so every Version of it goes.
    fn reclaim(&mut self, logical: &[u8]) -> bool {
        if let Some((namespace_id, collection_id)) = keys::data_collection(logical)
            && (self.dead_namespaces.contains(&namespace_id)
                || self
                    .dead_collections
                    .contains(&(namespace_id, collection_id)))
        {
            self.reclaimed_collections
                .insert((namespace_id, collection_id));
            true
        } else if let Some(index) = keys::index_entry_ids(logical)
            && self.dead_indexes.contains(&index)
        {
            self.reclaimed_indexes.insert(index);
            true
        } else if let Some(namespace_id) = keys::collection_entry_namespace(logical)
            && self.dead_namespaces.contains(&namespace_id)
        {
            self.uncached.insert(logical.to_vec());
            true
        } else if let Some((namespace_id, collection_id, _)) = keys::index_catalog_ids(logical)
            && (self.dead_namespaces.contains(&namespace_id)
                || self
                    .dead_collections
                    .contains(&(namespace_id, collection_id)))
        {
            self.uncached.insert(logical.to_vec());
            true
        } else {
            false
        }
    }

    /// Record the ID a deleted Catalog Version of `logical` frees: the
    /// Namespace, Collection or Index whose data is now dead.
    fn note_drop(&mut self, logical: &[u8], value: &[u8]) -> Result<()> {
        // Every Version older than a deleted one is deleted too, so the one
        // holding the dropped ID is reached here.
        let collection_namespace = keys::collection_entry_namespace(logical);
        if collection_namespace.is_some() || keys::is_namespace_entry(logical) {
            if self.dead_id_follows {
                self.dead_id_follows = false;
                let id = mvcc::decode_value(value)?
                    .map(|bytes| catalog::entry_id(&bytes))
                    .transpose()?;
                match (id, collection_namespace) {
                    (Some(id), Some(namespace_id)) => {
                        self.dead_collections.insert((namespace_id, id));
                    }
                    (Some(id), None) => {
                        self.dead_namespaces.insert(id);
                    }
                    (None, _) => {}
                }
            }
            self.dead_id_follows = mvcc::is_tombstone(value);
        }
        if let Some(index) = keys::index_catalog_ids(logical)
            && mvcc::is_tombstone(value)
        {
            self.dead_indexes.insert(index);
        }
        Ok(())
    }

    /// Delete `key` in `batch`, or hold it back if it is a Catalog key.
    fn delete(&mut self, key: &[u8], logical: &[u8], batch: &mut WriteBatch) {
        if keys::is_system_key(logical) {
            self.held_back.push(key.to_vec());
        } else {
            batch.delete(key.to_vec());
        }
    }

    /// Delete the held-back Catalog keys, now that their data is gone, and
    /// return what the pass removed.
    fn finish(&mut self) -> Result<GcStats> {
        // One batch, so no Catalog Tombstone is deleted apart from the
        // Versions under it; the Catalog is small.
        let mut catalog = WriteBatch::new();
        for key in std::mem::take(&mut self.held_back) {
            catalog.delete(key);
        }
        if !catalog.ops().is_empty() {
            self.node.engine.write_batch(catalog)?;
        }
        self.node.catalog_cache.remove(&self.uncached);
        self.node.catalog_cache.purge_tombstones(self.horizon);
        self.stats.collections_reclaimed = self.reclaimed_collections.len() as u64;
        self.stats.indexes_reclaimed = self.reclaimed_indexes.len() as u64;
        self.done = true;
        Ok(self.stats)
    }
}

#[cfg(test)]
mod tests {
    use bson::doc;
    use psdb_storage_memory::MemoryEngine;

    use super::*;
    use crate::{CollectionPath, FieldSchema, FieldType};

    fn shop() -> (Node<MemoryEngine>, CollectionPath) {
        let node = Node::new(MemoryEngine::new()).unwrap();
        node.create_namespace("shop").unwrap();
        let orders: CollectionPath = "shop.orders".parse().unwrap();
        node.create_collection(&orders).unwrap();
        (node, orders)
    }

    fn ids(node: &Node<MemoryEngine>, collection: &CollectionPath) -> (u32, u32) {
        node.begin_transaction()
            .resolve_collection(collection)
            .unwrap()
    }

    /// The key prefix of `collection`'s data.
    fn prefix(node: &Node<MemoryEngine>, collection: &CollectionPath) -> Vec<u8> {
        let (namespace_id, id) = ids(node, collection);
        keys::collection_prefix(namespace_id, id)
    }

    /// Declare `path` an Int field, strict if `unique`, and index it.
    fn index(node: &Node<MemoryEngine>, collection: &CollectionPath, path: &str, unique: bool) {
        let path = path.parse().unwrap();
        let field = FieldSchema {
            field_type: FieldType::Int,
            strict: unique,
        };
        node.set_field_type(collection, &path, field).unwrap();
        node.create_index(collection, &path, unique).unwrap();
    }

    /// Physical keys under `prefix`, whatever their timestamp.
    fn versions_under(node: &Node<MemoryEngine>, prefix: &[u8]) -> usize {
        let mut n = 0;
        node.engine
            .scan(&KeyRange::prefix(prefix), &mut |_, _| {
                n += 1;
                ControlFlow::Continue(())
            })
            .unwrap();
        n
    }

    #[test]
    #[should_panic(expected = "stepped a finished")]
    fn a_finished_pass_cannot_be_stepped() {
        let (node, _) = shop();
        let mut pass = node.start_garbage_collection();
        while pass.step().unwrap().is_none() {}
        let _ = pass.step();
    }

    #[test]
    fn a_batch_never_splits_a_tombstone_from_the_versions_under_it() {
        let (node, orders) = shop();
        // A deleted Document with more Versions than a batch, so a batch
        // boundary falls among them, after its Tombstone.
        node.insert(&orders, doc! { "_id": 2, "n": 0 }).unwrap();
        for n in 1..1100 {
            node.replace(&orders, &2.into(), doc! { "_id": 2, "n": n })
                .unwrap();
        }
        node.delete(&orders, &2.into()).unwrap();
        // A later key, so the batch closes before the pass ends.
        node.insert(&orders, doc! { "_id": 3 }).unwrap();

        let mut pass = node.start_garbage_collection();
        assert_eq!(pass.step().unwrap(), None, "the first batch filled up");
        assert_eq!(node.get(&orders, &2.into()).unwrap(), None);
    }

    /// Every physical key a pass scans, and how many are Tombstones:
    /// counted here, not by the pass, to check what it reports.
    fn scanned_and_tombstones(node: &Node<MemoryEngine>) -> (u64, u64) {
        let (mut keys, mut tombstones) = (0, 0);
        let all = KeyRange {
            start: keys::namespaces_prefix(),
            end: None,
        };
        node.engine
            .scan(&all, &mut |_, value| {
                keys += 1;
                tombstones += u64::from(mvcc::is_tombstone(value));
                ControlFlow::Continue(())
            })
            .unwrap();
        (keys, tombstones)
    }

    #[test]
    fn a_step_visits_exactly_its_batch_of_keys() {
        let (node, orders) = shop();
        node.run(|t| {
            for i in 0..95 {
                t.insert(&orders, doc! { "_id": i })?;
            }
            Ok(())
        })
        .unwrap();
        // One Version per key, so each step but the last visits exactly 10.
        let (keys, _) = scanned_and_tombstones(&node);
        let mut pass = node.start_garbage_collection().with_batch_keys(10);
        let mut steps = 1;
        while pass.step().unwrap().is_none() {
            steps += 1;
        }
        assert_eq!(steps, keys.div_ceil(10));
    }

    #[test]
    fn a_step_with_nothing_to_delete_stops_after_a_batch_of_keys() {
        let (node, orders) = shop();
        node.run(|t| {
            for i in 0..2000 {
                t.insert(&orders, doc! { "_id": i })?;
            }
            Ok(())
        })
        .unwrap();

        let mut pass = node.start_garbage_collection();
        assert_eq!(pass.step().unwrap(), None, "the scan stopped partway");
        while pass.step().unwrap().is_none() {}
    }

    #[test]
    fn a_dropped_collections_documents_and_index_entries_are_reclaimed() {
        let (node, orders) = shop();
        index(&node, &orders, "n", false);
        node.insert(&orders, doc! { "_id": 1, "n": 1 }).unwrap();
        node.insert(&orders, doc! { "_id": 2, "n": 2 }).unwrap();
        // Its Tombstone goes with the Collection, and must be counted.
        node.insert(&orders, doc! { "_id": 3, "n": 3 }).unwrap();
        node.delete(&orders, &3.into()).unwrap();
        let (namespace_id, id) = ids(&node, &orders);
        node.drop_collection(&orders).unwrap();

        let (keys_before, tombstones_before) = scanned_and_tombstones(&node);
        let stats = node.collect_garbage().unwrap();
        let (keys_after, tombstones_after) = scanned_and_tombstones(&node);
        assert_eq!(stats.versions_removed, keys_before - keys_after);
        assert_eq!(
            stats.tombstones_removed,
            tombstones_before - tombstones_after
        );
        assert!(stats.versions_removed > stats.tombstones_removed);
        assert_eq!(stats.collections_reclaimed, 1);
        assert_eq!(stats.indexes_reclaimed, 0, "counted with its Collection");
        assert_eq!(
            versions_under(&node, &keys::collection_prefix(namespace_id, id)),
            0
        );
        let index_entry = keys::index_catalog_key(namespace_id, id, 0);
        assert_eq!(versions_under(&node, &index_entry), 0);
        assert_eq!(
            node.catalog_cache.read_at(&index_entry, u64::MAX),
            catalog::Cached::Known(None)
        );
        assert_eq!(node.collect_garbage().unwrap(), GcStats::default());
    }

    #[test]
    fn an_open_snapshot_keeps_a_dropped_collection() {
        let (node, orders) = shop();
        node.insert(&orders, doc! { "_id": 1 }).unwrap();
        let data = prefix(&node, &orders);
        let mut reader = node.begin_transaction();
        node.drop_collection(&orders).unwrap();

        assert_eq!(node.collect_garbage().unwrap().collections_reclaimed, 0);
        assert_eq!(
            reader.get(&orders, &1.into()).unwrap(),
            Some(doc! { "_id": 1 })
        );

        drop(reader);
        assert_eq!(node.collect_garbage().unwrap().collections_reclaimed, 1);
        assert_eq!(versions_under(&node, &data), 0);
    }

    #[test]
    fn a_collection_created_after_the_horizon_is_untouched() {
        let (node, orders) = shop();
        node.insert(&orders, doc! { "_id": 1 }).unwrap();
        node.drop_collection(&orders).unwrap();
        let _reader = node.begin_transaction();
        node.create_collection(&orders).unwrap();
        node.insert(&orders, doc! { "_id": 2 }).unwrap();

        assert_eq!(node.collect_garbage().unwrap().collections_reclaimed, 1);
        assert_eq!(
            node.get(&orders, &2.into()).unwrap(),
            Some(doc! { "_id": 2 })
        );
    }

    #[test]
    fn an_older_drop_of_a_recreated_collection_is_reclaimed() {
        let (node, orders) = shop();
        node.insert(&orders, doc! { "_id": 1 }).unwrap();
        let old = prefix(&node, &orders);
        node.drop_collection(&orders).unwrap();
        node.create_collection(&orders).unwrap();
        node.insert(&orders, doc! { "_id": 2 }).unwrap();

        assert_eq!(node.collect_garbage().unwrap().collections_reclaimed, 1);
        assert_eq!(versions_under(&node, &old), 0);
        assert_eq!(
            node.get(&orders, &2.into()).unwrap(),
            Some(doc! { "_id": 2 })
        );
    }

    #[test]
    fn a_dropped_index_is_reclaimed_and_other_indexes_are_untouched() {
        let (node, orders) = shop();
        index(&node, &orders, "n", false);
        index(&node, &orders, "m", false);
        node.insert(&orders, doc! { "_id": 1, "n": 1, "m": 1 })
            .unwrap();
        let (namespace_id, id) = ids(&node, &orders);
        node.drop_index(&orders, &"n".parse().unwrap()).unwrap();

        let stats = node.collect_garbage().unwrap();
        assert_eq!(stats.indexes_reclaimed, 1);
        assert_eq!(stats.collections_reclaimed, 0);
        assert_eq!(
            versions_under(&node, &keys::index_prefix(namespace_id, id, 0)),
            0
        );
        assert_eq!(
            versions_under(&node, &keys::index_catalog_key(namespace_id, id, 0)),
            0
        );
        assert_eq!(
            versions_under(&node, &keys::index_prefix(namespace_id, id, 1)),
            1
        );
        assert_eq!(node.collect_garbage().unwrap(), GcStats::default());
    }

    #[test]
    fn a_failed_build_leaves_no_entries() {
        let (node, orders) = shop();
        node.run(|t| {
            // The duplicate sorts last, so the first chunk commits its entries.
            for i in 0..1100 {
                t.insert(&orders, doc! { "_id": i, "n": i })?;
            }
            t.insert(&orders, doc! { "_id": 5000, "n": 0 })?;
            Ok(())
        })
        .unwrap();
        let path = "n".parse().unwrap();
        let field = FieldSchema {
            field_type: FieldType::Int,
            strict: true,
        };
        node.set_field_type(&orders, &path, field).unwrap();
        let err = node.create_index(&orders, &path, true).unwrap_err();
        assert!(matches!(err, crate::Error::DuplicateValue(_)));
        let (namespace_id, id) = ids(&node, &orders);
        assert!(versions_under(&node, &keys::index_prefix(namespace_id, id, 0)) > 0);

        assert_eq!(node.collect_garbage().unwrap().indexes_reclaimed, 1);
        assert_eq!(
            versions_under(&node, &keys::index_prefix(namespace_id, id, 0)),
            0
        );
    }

    #[test]
    fn a_dropped_namespace_is_reclaimed_with_its_collections() {
        let (node, orders) = shop();
        let users: CollectionPath = "shop.users".parse().unwrap();
        node.create_collection(&users).unwrap();
        index(&node, &orders, "n", false);
        node.insert(&orders, doc! { "_id": 1, "n": 1 }).unwrap();
        node.insert(&users, doc! { "_id": 1 }).unwrap();
        let (namespace_id, orders_id) = ids(&node, &orders);
        let (_, users_id) = ids(&node, &users);
        node.drop_namespace("shop").unwrap();

        assert_eq!(node.collect_garbage().unwrap().collections_reclaimed, 2);
        for id in [orders_id, users_id] {
            assert_eq!(
                versions_under(&node, &keys::collection_prefix(namespace_id, id)),
                0
            );
        }
        for entry in [
            keys::collection_entry_key(namespace_id, "orders"),
            keys::collection_entry_key(namespace_id, "users"),
            keys::index_catalog_key(namespace_id, orders_id, 0),
        ] {
            assert_eq!(versions_under(&node, &entry), 0);
            assert_eq!(
                node.catalog_cache.read_at(&entry, u64::MAX),
                catalog::Cached::Known(None)
            );
        }
        assert_eq!(
            versions_under(&node, &keys::namespace_id_key(namespace_id)),
            1
        );
        assert_eq!(node.collect_garbage().unwrap(), GcStats::default());
    }
}
