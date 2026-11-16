//! Garbage Collection of old Versions (ADR 0012).

use std::collections::HashSet;
use std::ops::ControlFlow;

use psdb_storage::{KeyRange, StorageEngine, WriteBatch};

use crate::catalog;
use crate::error::Result;
use crate::keys;
use crate::mvcc;
use crate::node::Node;

/// The most physical keys one pass deletes in a single batch.
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
}

impl<E: StorageEngine> Node<E> {
    /// Remove every Version no Snapshot at or after the Horizon can read:
    /// for each key, the Versions older than its newest one at the Horizon,
    /// and that one too if it is a Tombstone. Deletes straight from the
    /// engine in batches, without the commit lock and without logging, so
    /// Recovery may bring removed Versions back for the next pass to remove.
    ///
    /// Deleting the Tombstone of a Collection's Catalog entry makes its ID
    /// dead: every key under it is deleted, whatever its timestamp, since
    /// IDs are never reused. The Catalog sorts before the data, so dead IDs
    /// are known before the scan reaches it.
    ///
    /// One pass runs at a time; a second call waits for the first.
    pub fn collect_garbage(&self) -> Result<GcStats> {
        let _one_pass = self.gc_lock.lock().expect("gc lock poisoned");
        let horizon = self.horizon();
        let mut stats = GcStats::default();
        // Unversioned state sorts below the Namespaces, so it is skipped.
        let mut range = KeyRange {
            start: keys::namespaces_prefix(),
            end: None,
        };
        // The logical key being visited, and whether its newest Version at
        // the Horizon was already seen. Carried across batches, which can
        // split one key's Versions.
        let mut current: Vec<u8> = Vec::new();
        let mut seen_newest = false;
        // Set on deleting a Collection entry's Tombstone: the key's next
        // Version holds the dropped ID.
        let mut dead_id_follows = false;
        // (Namespace ID, Collection ID) of each dropped Collection found.
        let mut dead_collections = HashSet::new();
        let mut reclaimed_collections = HashSet::new();
        // Catalog keys to delete once the data is gone, so a crash never
        // loses the Tombstone that marks data dead.
        let mut held_back = Vec::new();
        loop {
            let mut batch = WriteBatch::new();
            let mut next_start = None;
            let mut failure = None;
            self.engine.scan(&range, &mut |key, value| {
                if batch.ops().len() == BATCH_KEYS {
                    next_start = Some(key.to_vec());
                    return ControlFlow::Break(());
                }
                let (logical, ts) = match keys::split_physical(key) {
                    Ok(parts) => parts,
                    Err(e) => {
                        failure = Some(e);
                        return ControlFlow::Break(());
                    }
                };
                if logical != current.as_slice() {
                    current = logical.to_vec();
                    seen_newest = false;
                    dead_id_follows = false;
                }
                if let Some(collection) = keys::data_collection(logical)
                    && dead_collections.contains(&collection)
                {
                    reclaimed_collections.insert(collection);
                    stats.versions_removed += 1;
                    stats.tombstones_removed += u64::from(mvcc::is_tombstone(value));
                    batch.delete(key.to_vec());
                    return ControlFlow::Continue(());
                }
                if ts > horizon {
                    return ControlFlow::Continue(());
                }
                if !seen_newest {
                    seen_newest = true;
                    if !mvcc::is_tombstone(value) {
                        return ControlFlow::Continue(());
                    }
                    stats.tombstones_removed += 1;
                }
                stats.versions_removed += 1;
                // Every Version older than a deleted one is deleted too, so
                // the one holding the dropped ID is reached here.
                if let Some(namespace_id) = keys::collection_entry_namespace(logical) {
                    if dead_id_follows {
                        dead_id_follows = false;
                        let id = mvcc::decode_value(value)
                            .and_then(|v| v.map(|bytes| catalog::entry_id(&bytes)).transpose());
                        match id {
                            Ok(Some(id)) => {
                                dead_collections.insert((namespace_id, id));
                            }
                            Ok(None) => {}
                            Err(e) => {
                                failure = Some(e);
                                return ControlFlow::Break(());
                            }
                        }
                    }
                    dead_id_follows = mvcc::is_tombstone(value);
                }
                if keys::is_system_key(logical) {
                    held_back.push(key.to_vec());
                } else {
                    batch.delete(key.to_vec());
                }
                ControlFlow::Continue(())
            })?;
            if let Some(e) = failure {
                return Err(e);
            }
            if !batch.ops().is_empty() {
                self.engine.write_batch(batch)?;
            }
            match next_start {
                Some(start) => range.start = start,
                None => break,
            }
        }
        let mut held_back = held_back.into_iter().peekable();
        while held_back.peek().is_some() {
            let mut batch = WriteBatch::new();
            for key in held_back.by_ref().take(BATCH_KEYS) {
                batch.delete(key);
            }
            self.engine.write_batch(batch)?;
        }
        self.catalog_cache.purge_tombstones(horizon);
        stats.collections_reclaimed = reclaimed_collections.len() as u64;
        Ok(stats)
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

    /// The key prefix of `collection`'s data.
    fn prefix(node: &Node<MemoryEngine>, collection: &CollectionPath) -> Vec<u8> {
        let (namespace_id, id) = node
            .begin_transaction()
            .resolve_collection(collection)
            .unwrap();
        keys::collection_prefix(namespace_id, id)
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
    fn a_dropped_collections_documents_and_index_entries_are_reclaimed() {
        let (node, orders) = shop();
        let n = "n".parse().unwrap();
        let field = FieldSchema {
            field_type: FieldType::Int,
            strict: false,
        };
        node.set_field_type(&orders, &n, field).unwrap();
        node.create_index(&orders, &n, false).unwrap();
        node.insert(&orders, doc! { "_id": 1, "n": 1 }).unwrap();
        node.insert(&orders, doc! { "_id": 2, "n": 2 }).unwrap();
        let data = prefix(&node, &orders);
        node.drop_collection(&orders).unwrap();

        let stats = node.collect_garbage().unwrap();
        assert_eq!(stats.collections_reclaimed, 1);
        assert_eq!(versions_under(&node, &data), 0);
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
}
