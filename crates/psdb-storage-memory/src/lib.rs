//! An in-memory [`StorageEngine`] backed by a `BTreeMap`.
//!
//! Nothing reaches disk: a restart loses everything unless the Database
//! Layer's WAL replays it (ADR 0004).

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::RwLock;

use psdb_storage::{KeyRange, Result, StorageEngine, WriteBatch, WriteOp};

/// The entries a [`MemoryScan`] copies per read lock.
const SCAN_BUFFER: usize = 64;

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

    type Scan<'a> = MemoryScan<'a>;

    fn scan(&self, range: &KeyRange) -> MemoryScan<'_> {
        MemoryScan {
            engine: self,
            from: Bound::Included(range.start.clone()),
            end: range.end.clone(),
            buffer: Vec::new().into_iter(),
            done: range.end.as_ref().is_some_and(|end| *end <= range.start),
        }
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

/// The iterator [`MemoryEngine::scan`] returns. It copies a few entries
/// at a time under the read lock and holds no lock between `next` calls,
/// so a caller that pauses never blocks writers (ADR 0018).
#[derive(Debug)]
pub struct MemoryScan<'a> {
    engine: &'a MemoryEngine,
    /// Where the next copy starts: the range's start, then just past the
    /// last key copied.
    from: Bound<Vec<u8>>,
    end: Option<Vec<u8>>,
    buffer: std::vec::IntoIter<(Vec<u8>, Vec<u8>)>,
    /// Whether the last copy reached the end of the range.
    done: bool,
}

impl Iterator for MemoryScan<'_> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.buffer.len() == 0 && !self.done {
            self.refill();
        }
        self.buffer.next().map(Ok)
    }
}

impl MemoryScan<'_> {
    /// Copy the next [`SCAN_BUFFER`] entries under the read lock.
    fn refill(&mut self) {
        let map = self.engine.map.read().expect("lock poisoned");
        let end = self
            .end
            .as_deref()
            .map_or(Bound::Unbounded, Bound::Excluded);
        let from = self.from.as_ref().map(Vec::as_slice);
        let copied: Vec<_> = map
            .range::<[u8], _>((from, end))
            .take(SCAN_BUFFER)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        self.done = copied.len() < SCAN_BUFFER;
        if let Some((last, _)) = copied.last() {
            self.from = Bound::Excluded(last.clone());
        }
        self.buffer = copied.into_iter();
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
