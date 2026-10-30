//! A simulated disk for testing the WAL against power loss and I/O errors.

use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

use super::WalFile;

/// A simulated disk holding one WAL file, for tests.
///
/// It tracks which bytes have been synced, so [`power_loss`](Self::power_loss)
/// can drop everything else, as a real power cut drops the OS page cache.
/// A process kill cannot test this: the OS keeps unsynced bytes and writes
/// them later.
///
/// `SimDisk` is a cheap handle: clones share one disk. Hand
/// [`file`](Self::file) to [`Database::open_with`](crate::Database::open_with),
/// crash it, then open a new `file` to recover. After a power loss, files
/// handed out earlier fail every call, like a process that died with the
/// machine.
///
/// ```
/// use psdb_core::wal::{SimDisk, WalFile};
///
/// let disk = SimDisk::new();
/// let mut file = disk.file();
/// file.append(b"synced")?;
/// file.sync()?;
/// file.sync_dir()?;
/// file.append(b" lost")?;
/// disk.power_loss(0);
/// assert_eq!(disk.contents(), b"synced");
/// assert!(file.append(b"x").is_err(), "the old process is gone");
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Clone, Default)]
pub struct SimDisk {
    state: Arc<Mutex<DiskState>>,
}

#[derive(Default)]
struct DiskState {
    /// The file as the running process sees it.
    data: Vec<u8>,
    /// The file as of the last sync: what survives power loss.
    durable: Vec<u8>,
    /// Whether the file's directory entry survives power loss.
    entry_durable: bool,
    /// Bumped by each power loss; files from older generations are dead.
    generation: u64,
    /// `(ops left, unsynced bytes kept)`: crash on the operation after
    /// `ops left` more succeed.
    crash: Option<(usize, usize)>,
    fail_next_append: bool,
    fail_next_sync: bool,
}

impl DiskState {
    fn power_loss(&mut self, keep_unsynced: usize) {
        if !self.entry_durable {
            self.durable.clear();
        } else if self.data.starts_with(&self.durable) {
            // A torn write: some of the unsynced bytes happened to reach
            // the disk.
            let unsynced = &self.data[self.durable.len()..];
            let kept = &unsynced[..keep_unsynced.min(unsynced.len())];
            self.durable.extend_from_slice(kept);
        }
        self.data = self.durable.clone();
        self.generation += 1;
        self.crash = None;
        self.fail_next_append = false;
        self.fail_next_sync = false;
    }

    /// Count one operation towards a scheduled crash.
    fn tick(&mut self) -> io::Result<()> {
        match self.crash {
            Some((0, keep_unsynced)) => {
                self.power_loss(keep_unsynced);
                Err(io::Error::other("simulated power loss"))
            }
            Some((left, keep_unsynced)) => {
                self.crash = Some((left - 1, keep_unsynced));
                Ok(())
            }
            None => Ok(()),
        }
    }
}

impl SimDisk {
    /// A disk with no WAL file on it yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Open the WAL file, creating it if it does not exist.
    pub fn file(&self) -> SimWalFile {
        SimWalFile {
            disk: self.clone(),
            generation: self.lock().generation,
        }
    }

    /// Cut the power now. The file reverts to its last synced contents plus
    /// up to `keep_unsynced` of the bytes appended since, and vanishes
    /// altogether if its directory entry was never synced.
    pub fn power_loss(&self, keep_unsynced: usize) {
        self.lock().power_loss(keep_unsynced);
    }

    /// Cut the power during an operation (append, sync, truncate or
    /// directory sync) after `ops` more of them succeed, as in
    /// [`power_loss`](Self::power_loss). An append that crashes still
    /// counts its bytes as appended, so they may be kept torn.
    pub fn crash_after(&self, ops: usize, keep_unsynced: usize) {
        self.lock().crash = Some((ops, keep_unsynced));
    }

    /// Make the next append fail without writing anything.
    pub fn fail_next_append(&self) {
        self.lock().fail_next_append = true;
    }

    /// Make the next sync fail. The bytes it should have synced stay
    /// unsynced, as after a real fsync failure.
    pub fn fail_next_sync(&self) {
        self.lock().fail_next_sync = true;
    }

    /// The file as the running process sees it.
    pub fn contents(&self) -> Vec<u8> {
        self.lock().data.clone()
    }

    /// Replace the file with `bytes`, already durable.
    pub fn set_contents(&self, bytes: Vec<u8>) {
        let mut state = self.lock();
        state.data = bytes.clone();
        state.durable = bytes;
        state.entry_durable = true;
    }

    fn lock(&self) -> MutexGuard<'_, DiskState> {
        self.state.lock().expect("lock poisoned")
    }
}

/// The WAL file on a [`SimDisk`].
pub struct SimWalFile {
    disk: SimDisk,
    generation: u64,
}

impl SimWalFile {
    /// The disk state, unless the machine lost power since this file was
    /// opened.
    fn live(&self) -> io::Result<MutexGuard<'_, DiskState>> {
        let state = self.disk.lock();
        if state.generation != self.generation {
            return Err(io::Error::other("the machine lost power"));
        }
        Ok(state)
    }
}

impl WalFile for SimWalFile {
    fn read_all(&mut self) -> io::Result<Vec<u8>> {
        Ok(self.live()?.data.clone())
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut state = self.live()?;
        if std::mem::take(&mut state.fail_next_append) {
            return Err(io::Error::other("injected append failure"));
        }
        state.data.extend_from_slice(bytes);
        state.tick()
    }

    fn sync(&mut self) -> io::Result<()> {
        let mut state = self.live()?;
        if std::mem::take(&mut state.fail_next_sync) {
            return Err(io::Error::other("injected fsync failure"));
        }
        state.tick()?;
        state.durable = state.data.clone();
        Ok(())
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        let mut state = self.live()?;
        state.tick()?;
        let len = usize::try_from(len).expect("simulated files fit in memory");
        state.data.truncate(len);
        Ok(())
    }

    fn sync_dir(&mut self) -> io::Result<()> {
        let mut state = self.live()?;
        state.tick()?;
        state.entry_durable = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::conformance;

    #[test]
    fn passes_conformance_suite() {
        conformance::run_all(|| {
            let disk = SimDisk::new();
            move || disk.file()
        });
    }

    #[test]
    fn file_vanishes_unless_directory_synced() {
        let disk = SimDisk::new();
        let mut file = disk.file();
        file.append(b"data").unwrap();
        file.sync().unwrap();
        disk.power_loss(0);
        assert_eq!(disk.contents(), b"", "directory entry was never synced");
    }

    #[test]
    fn power_loss_keeps_a_torn_prefix() {
        let disk = SimDisk::new();
        let mut file = disk.file();
        file.append(b"ab").unwrap();
        file.sync().unwrap();
        file.sync_dir().unwrap();
        file.append(b"cdef").unwrap();
        disk.power_loss(2);
        assert_eq!(disk.contents(), b"abcd");
    }

    #[test]
    fn failed_sync_leaves_bytes_unsynced() {
        let disk = SimDisk::new();
        let mut file = disk.file();
        file.sync_dir().unwrap();
        file.append(b"x").unwrap();
        disk.fail_next_sync();
        assert!(file.sync().is_err());
        disk.power_loss(0);
        assert_eq!(disk.contents(), b"");
    }

    #[test]
    fn scheduled_crash_fails_the_operation_and_kills_old_files() {
        let disk = SimDisk::new();
        let mut file = disk.file();
        file.sync_dir().unwrap();
        disk.crash_after(1, usize::MAX);
        file.append(b"kept").unwrap();
        assert!(file.append(b"torn").is_err());
        assert_eq!(disk.contents(), b"kepttorn", "both unsynced appends kept");
        assert!(file.sync().is_err(), "the old process is gone");
        assert!(disk.file().read_all().is_ok());
    }
}
