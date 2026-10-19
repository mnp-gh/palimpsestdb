//! An in-memory [`StorageEngine`] backed by a `BTreeMap`.
//!
//! Nothing reaches disk: a restart loses everything unless the Document
//! Layer's WAL replays it (ADR 0004).

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::RwLock;

use psdb_storage::{KeyRange, Result, ScanVisitor, StorageEngine, WriteBatch, WriteOp};

/// An in-memory Storage Engine.
///
/// Batches are atomic because each one is applied while holding the write
/// lock, so readers see all of a batch or none of it.
#[derive(Debug, Default)]
pub struct MemoryEngine {
    map: RwLock<BTreeMap<Vec<u8>, Vec<u8>>>,
}

impl MemoryEngine {
    /// An empty engine.
    pub fn new() -> Self {
        Self::default()
    }
}

impl StorageEngine for MemoryEngine {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.map.read().expect("lock poisoned").get(key).cloned())
    }

    fn scan(&self, range: &KeyRange, visit: &mut ScanVisitor<'_>) -> Result<()> {
        let map = self.map.read().expect("lock poisoned");
        let end = match &range.end {
            Some(end) if *end <= range.start => return Ok(()),
            Some(end) => Bound::Excluded(end.as_slice()),
            None => Bound::Unbounded,
        };
        let bounds = (Bound::Included(range.start.as_slice()), end);
        for (key, value) in map.range::<[u8], _>(bounds) {
            if visit(key, value).is_break() {
                break;
            }
        }
        Ok(())
    }

    fn write_batch(&self, batch: WriteBatch) -> Result<()> {
        let mut map = self.map.write().expect("lock poisoned");
        for op in batch.into_ops() {
            match op {
                WriteOp::Put { key, value } => {
                    map.insert(key, value);
                }
                WriteOp::Delete { key } => {
                    map.remove(&key);
                }
            }
        }
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::MemoryEngine;

    #[test]
    fn passes_conformance_suite() {
        psdb_storage::conformance::run_all(MemoryEngine::new);
    }
}
