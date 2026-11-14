//! Garbage Collection of old Versions (ADR 0012).

use std::ops::ControlFlow;

use psdb_storage::{KeyRange, StorageEngine, WriteBatch};

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
}

impl<E: StorageEngine> Node<E> {
    /// Remove every Version no Snapshot at or after the Horizon can read:
    /// for each key, the Versions older than its newest one at the Horizon,
    /// and that one too if it is a Tombstone. Deletes straight from the
    /// engine in batches, without the commit lock and without logging, so
    /// Recovery may bring removed Versions back for the next pass to remove.
    pub fn collect_garbage(&self) -> Result<GcStats> {
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
                batch.delete(key.to_vec());
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
        self.catalog_cache.purge_tombstones(horizon);
        Ok(stats)
    }
}
