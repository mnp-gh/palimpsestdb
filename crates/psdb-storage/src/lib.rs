//! The Storage Engine contract for PalimpsestDB.
//!
//! A Storage Engine is an ordered store of byte keys to byte values with
//! atomic batched writes. It knows nothing about Documents, Versions or
//! Transactions: those live in the Document Layer (see ADR 0001).
//!
//! Every implementation must pass [`conformance::run_all`].

use std::fmt;
use std::ops::ControlFlow;

pub mod conformance;

/// A half-open range of keys, `[start, end)`, compared byte by byte.
///
/// `end: None` means "to the end of the keyspace".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRange {
    /// First key included in the range.
    pub start: Vec<u8>,
    /// First key excluded from the range, or `None` for unbounded.
    pub end: Option<Vec<u8>>,
}

impl KeyRange {
    /// The range `[start, end)`.
    pub fn new(start: impl Into<Vec<u8>>, end: impl Into<Vec<u8>>) -> Self {
        Self {
            start: start.into(),
            end: Some(end.into()),
        }
    }

    /// Every key.
    pub fn all() -> Self {
        Self {
            start: Vec::new(),
            end: None,
        }
    }

    /// Every key that starts with `prefix`.
    ///
    /// ```
    /// use psdb_storage::KeyRange;
    /// let r = KeyRange::prefix(b"ab");
    /// assert!(r.contains(b"ab") && r.contains(b"ab\xff\xff"));
    /// assert!(!r.contains(b"ac") && !r.contains(b"a"));
    /// ```
    pub fn prefix(prefix: &[u8]) -> Self {
        Self {
            start: prefix.to_vec(),
            end: prefix_successor(prefix),
        }
    }

    /// Whether `key` falls inside this range.
    pub fn contains(&self, key: &[u8]) -> bool {
        key >= self.start.as_slice() && self.end.as_deref().is_none_or(|end| key < end)
    }
}

/// The smallest key greater than every key starting with `prefix`, or `None`
/// if no such key exists (the prefix is empty or all `0xFF`).
fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < 0xFF {
            end.push(last + 1);
            return Some(end);
        }
    }
    None
}

/// One write inside a [`WriteBatch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOp {
    /// Set `key` to `value`, replacing any existing value.
    Put {
        /// The key to write.
        key: Vec<u8>,
        /// The value to store.
        value: Vec<u8>,
    },
    /// Remove `key`. Deleting a missing key is not an error.
    Delete {
        /// The key to remove.
        key: Vec<u8>,
    },
}

/// An ordered list of writes applied atomically by
/// [`StorageEngine::write_batch`]. When two ops touch the same key, the later
/// one wins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteBatch {
    ops: Vec<WriteOp>,
}

impl WriteBatch {
    /// An empty batch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a put.
    pub fn put(&mut self, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> &mut Self {
        self.ops.push(WriteOp::Put {
            key: key.into(),
            value: value.into(),
        });
        self
    }

    /// Append a delete.
    pub fn delete(&mut self, key: impl Into<Vec<u8>>) -> &mut Self {
        self.ops.push(WriteOp::Delete { key: key.into() });
        self
    }

    /// The ops in the order they were added.
    pub fn ops(&self) -> &[WriteOp] {
        &self.ops
    }

    /// Consume the batch, yielding its ops in order.
    pub fn into_ops(self) -> Vec<WriteOp> {
        self.ops
    }

    /// Number of ops in the batch.
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Whether the batch has no ops.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

/// A failure inside a Storage Engine.
#[derive(Debug)]
pub enum StorageError {
    /// The underlying I/O failed.
    Io(std::io::Error),
    /// The engine found data it cannot interpret.
    Corruption(String),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::Io(e) => write!(f, "storage I/O error: {e}"),
            StorageError::Corruption(msg) => write!(f, "storage corruption: {msg}"),
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StorageError::Io(e) => Some(e),
            StorageError::Corruption(_) => None,
        }
    }
}

impl From<std::io::Error> for StorageError {
    fn from(e: std::io::Error) -> Self {
        StorageError::Io(e)
    }
}

/// The callback [`StorageEngine::scan`] calls with each key and value.
/// Return [`ControlFlow::Break`] to stop the scan.
pub type ScanVisitor<'a> = dyn FnMut(&[u8], &[u8]) -> ControlFlow<()> + 'a;

/// Result type for Storage Engine operations.
pub type Result<T> = std::result::Result<T, StorageError>;

/// The contract every Storage Engine must honour.
///
/// The Document Layer relies on each rule below; [`conformance::run_all`]
/// checks them.
///
/// 1. **Byte order.** Keys compare lexicographically as unsigned bytes
///    (like `memcmp`, shorter key first on a tie). [`scan`](Self::scan)
///    visits keys in ascending order.
/// 2. **Atomic batches.** After [`write_batch`](Self::write_batch) returns,
///    every op in the batch is visible to later reads. After a crash, either
///    the whole batch is present or none of it is.
/// 3. **Ordered batches.** Batches take effect in the order their
///    `write_batch` calls returned. After a crash, if a batch survived then
///    every earlier batch survived too.
/// 4. **Read your writes.** Reads reflect every batch whose `write_batch`
///    call has returned.
/// 5. **Durability is optional.** `write_batch` need not reach disk;
///    durability comes from the Document Layer's WAL (ADR 0004).
///    [`flush`](Self::flush) makes everything written so far durable, if the
///    engine is persistent at all.
///
/// Engines are `Send + Sync`: every method takes `&self`, so implementations
/// use interior mutability.
pub trait StorageEngine: Send + Sync {
    /// The value stored at `key`, if any.
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// Visit every key in `range` in ascending order, calling `visit` with
    /// each key and value. Return [`ControlFlow::Break`] from `visit` to stop
    /// early.
    ///
    /// `visit` must not call back into the engine.
    fn scan(&self, range: &KeyRange, visit: &mut ScanVisitor<'_>) -> Result<()>;

    /// Apply every op in `batch` atomically, in order.
    fn write_batch(&self, batch: WriteBatch) -> Result<()>;

    /// Make every batch written so far durable. A no-op for engines that
    /// keep nothing on disk.
    fn flush(&self) -> Result<()>;
}
