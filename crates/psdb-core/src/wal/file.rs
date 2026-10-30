//! The I/O seam under the WAL.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// The one file the WAL lives in, and the directory that holds it.
///
/// The WAL does all its I/O through this trait so tests can substitute
/// [`SimDisk`](super::SimDisk), which simulates power loss.
///
/// Contract:
///
/// 1. **Created on open.** An implementation opens the file, creating it
///    empty if it does not exist.
/// 2. **Append only.** [`append`](Self::append) writes at the end of the
///    file. The bytes are readable at once but not durable.
/// 3. **Sync.** Once [`sync`](Self::sync) returns `Ok`, every byte appended
///    and every truncation so far survives power loss, provided the file's
///    directory entry does.
/// 4. **Directory sync.** Once [`sync_dir`](Self::sync_dir) returns `Ok`,
///    the file's directory entry survives power loss. Until then a newly
///    created file may vanish.
/// 5. **No retries.** After any error, the state on disk is unknown.
pub trait WalFile: Send {
    /// The whole file.
    fn read_all(&mut self) -> io::Result<Vec<u8>>;

    /// Append `bytes` at the end of the file.
    fn append(&mut self, bytes: &[u8]) -> io::Result<()>;

    /// Make every append and truncation so far durable.
    fn sync(&mut self) -> io::Result<()>;

    /// Cut the file to `len` bytes.
    fn truncate(&mut self, len: u64) -> io::Result<()>;

    /// Make the file's directory entry durable.
    fn sync_dir(&mut self) -> io::Result<()>;
}

/// The WAL file `wal` in a directory on the real filesystem.
pub struct OsWalFile {
    file: File,
    dir: PathBuf,
}

impl OsWalFile {
    /// Open `dir/wal`, creating the file and, if needed, `dir` itself (but
    /// not `dir`'s parents).
    pub fn open(dir: impl AsRef<Path>) -> io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        if !dir.exists() {
            fs::create_dir(&dir)?;
            // Make the new directory's own entry durable.
            if let Some(parent) = dir.parent() {
                let parent = if parent.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    parent
                };
                sync_dir(parent)?;
            }
        }
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(dir.join("wal"))?;
        Ok(Self { file, dir })
    }
}

impl WalFile for OsWalFile {
    fn read_all(&mut self) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.file.seek(SeekFrom::Start(0))?;
        self.file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all(bytes)
    }

    fn sync(&mut self) -> io::Result<()> {
        // `F_FULLFSYNC` on Apple platforms, where plain `fsync` does not
        // flush the disk's cache (ADR 0004).
        self.file.sync_all()
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }

    fn sync_dir(&mut self) -> io::Result<()> {
        sync_dir(&self.dir)
    }
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Windows cannot open a directory as a file; NTFS journals directory
/// entries itself.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::wal::conformance;

    #[test]
    fn passes_conformance_suite() {
        let root = std::env::temp_dir().join(format!("psdb-wal-file-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        let next = Cell::new(0);
        conformance::run_all(|| {
            let dir = root.join(next.get().to_string());
            next.set(next.get() + 1);
            move || OsWalFile::open(&dir).unwrap()
        });
        fs::remove_dir_all(&root).unwrap();
    }
}
