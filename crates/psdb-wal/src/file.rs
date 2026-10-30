//! The file a [`Wal`](crate::Wal) writes to.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// An append-only file with explicit syncs.
///
/// The [`Wal`](crate::Wal) relies on each rule below.
///
/// 1. **Appends are ordered.** Bytes are added at the end, in the order
///    [`append`](Self::append) was called.
/// 2. **Sync makes durable.** After [`sync`](Self::sync) returns `Ok`, every
///    earlier append and truncate survives a crash.
/// 3. **A crash keeps a prefix.** After a crash, the file holds every byte
///    synced before it, possibly followed by some prefix of the bytes
///    appended after the last sync.
/// 4. **A failed sync decides nothing.** After `sync` fails, which unsynced
///    bytes survive a crash is unknown, and a later successful `sync` does
///    not change that.
pub trait LogFile {
    /// Append `bytes` at the end of the file. Not durable until
    /// [`sync`](Self::sync) returns.
    fn append(&mut self, bytes: &[u8]) -> io::Result<()>;

    /// Make every append and truncate so far durable.
    fn sync(&mut self) -> io::Result<()>;

    /// The whole file, synced or not.
    fn read_all(&mut self) -> io::Result<Vec<u8>>;

    /// Cut the file to its first `len` bytes. Not durable until
    /// [`sync`](Self::sync) returns.
    fn truncate(&mut self, len: u64) -> io::Result<()>;
}

/// A [`LogFile`] on the real filesystem.
#[derive(Debug)]
pub struct FsLogFile {
    file: File,
}

impl FsLogFile {
    /// Open the file at `path`, creating it if missing.
    pub fn open(path: &Path) -> io::Result<Self> {
        let created = !path.try_exists()?;
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)?;
        if created {
            sync_parent_dir(path)?;
        }
        Ok(Self { file })
    }
}

impl LogFile for FsLogFile {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all(bytes)
    }

    fn sync(&mut self) -> io::Result<()> {
        // `sync_all` issues F_FULLFSYNC on Apple platforms (ADR 0004).
        self.file.sync_all()
    }

    fn read_all(&mut self) -> io::Result<Vec<u8>> {
        // The file is opened in append mode, so moving the cursor here does
        // not move where appends land.
        self.file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        self.file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }
}

/// A new file's directory entry survives a crash only once its directory is
/// synced.
fn sync_parent_dir(path: &Path) -> io::Result<()> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    File::open(dir)?.sync_all()
}
