//! A [`LogFile`] for crash tests.

use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::LogFile;

/// An in-memory [`LogFile`] that simulates crashes, torn writes and I/O
/// failures.
///
/// Clones share one file, so a test can keep a handle after handing a clone
/// to a [`Wal`](crate::Wal), crash it, and reopen.
///
/// Truncation is durable at once; the `Wal` syncs after truncating anyway.
#[derive(Debug, Clone, Default)]
pub struct FaultyLogFile {
    state: Arc<Mutex<State>>,
}

#[derive(Debug, Default)]
struct State {
    bytes: Vec<u8>,
    synced_len: usize,
    fail_next_append: bool,
    fail_next_sync: bool,
}

impl FaultyLogFile {
    /// An empty file.
    pub fn new() -> Self {
        Self::default()
    }

    /// Simulate a crash: keep every synced byte and the first `torn`
    /// unsynced bytes (all of them if there are fewer), as rule 3 of
    /// [`LogFile`] allows.
    pub fn crash(&self, torn: usize) {
        let mut state = self.state();
        let keep = (state.synced_len + torn).min(state.bytes.len());
        state.bytes.truncate(keep);
        state.synced_len = keep;
    }

    /// The length of the file, synced or not.
    pub fn len(&self) -> usize {
        self.state().bytes.len()
    }

    /// Whether the file is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The number of bytes appended since the last sync.
    pub fn unsynced_len(&self) -> usize {
        let state = self.state();
        state.bytes.len() - state.synced_len
    }

    /// Make the next append write only the first half of its bytes and then
    /// fail.
    pub fn fail_next_append(&self) {
        self.state().fail_next_append = true;
    }

    /// Make the next sync fail without making anything durable.
    pub fn fail_next_sync(&self) {
        self.state().fail_next_sync = true;
    }

    /// Flip the lowest bit of the byte at `offset`, as a bad disk might.
    pub fn corrupt(&self, offset: usize) {
        self.state().bytes[offset] ^= 1;
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("lock poisoned")
    }
}

impl LogFile for FaultyLogFile {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut state = self.state();
        if std::mem::take(&mut state.fail_next_append) {
            state.bytes.extend_from_slice(&bytes[..bytes.len() / 2]);
            return Err(io::Error::other("injected append failure"));
        }
        state.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        let mut state = self.state();
        if std::mem::take(&mut state.fail_next_sync) {
            return Err(io::Error::other("injected sync failure"));
        }
        state.synced_len = state.bytes.len();
        Ok(())
    }

    fn read_all(&mut self) -> io::Result<Vec<u8>> {
        Ok(self.state().bytes.clone())
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        let mut state = self.state();
        let len = usize::try_from(len).map_err(io::Error::other)?;
        state.bytes.truncate(len);
        state.synced_len = state.synced_len.min(len);
        Ok(())
    }
}
