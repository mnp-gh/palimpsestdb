//! The write-ahead log of PalimpsestDB (ADR 0004, ADR 0010).
//!
//! The Document Layer appends one WAL Record per committed Transaction and
//! syncs it before the Transaction is Applied. On open, [`Wal::open`]
//! returns every intact Record so the Document Layer can run Recovery.
//!
//! The file starts with a header, `"PWAL" ‖ format version (u32 BE)`,
//! followed by Records back to back.

use std::fmt;
use std::io;

#[cfg(any(test, feature = "testing"))]
mod faulty;
mod file;
mod record;

#[cfg(any(test, feature = "testing"))]
pub use faulty::FaultyLogFile;
pub use file::{FsLogFile, LogFile};
pub use record::{Entry, Record};

const MAGIC: &[u8; 4] = b"PWAL";
const FORMAT_VERSION: u32 = 1;
const HEADER_LEN: usize = 8;

/// A failure in the WAL.
#[derive(Debug)]
pub enum Error {
    /// The underlying I/O failed.
    Io(io::Error),
    /// The file is not a WAL this version can read, or holds a Record that
    /// passed its checksum but cannot be decoded.
    Corruption(String),
    /// A Record, or one of its keys or values, is longer than 4 GiB.
    RecordTooLarge,
    /// An earlier append or sync failed, so whether its Record is in the
    /// file is unknown (ADR 0010). Reopen to find out.
    Failed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "WAL I/O error: {e}"),
            Error::Corruption(msg) => write!(f, "WAL corruption: {msg}"),
            Error::RecordTooLarge => write!(f, "WAL Record too large"),
            Error::Failed => write!(f, "an earlier WAL append or sync failed"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

/// Result type for WAL operations.
pub type Result<T> = std::result::Result<T, Error>;

/// A write-ahead log over a [`LogFile`].
#[derive(Debug)]
pub struct Wal<F: LogFile> {
    file: F,
    failed: bool,
}

impl<F: LogFile> Wal<F> {
    /// Open the WAL in `file` and return it with every intact Record,
    /// oldest first.
    ///
    /// An empty file gets a header. A torn last Record (cut short, or
    /// failing its checksum) is cut off the file: it was never synced, so
    /// its Transaction was never acknowledged.
    pub fn open(mut file: F) -> Result<(Self, Vec<Record>)> {
        let bytes = file.read_all()?;
        if bytes.len() < HEADER_LEN && header().starts_with(&bytes) {
            // A new file, or a crash while its header was being written.
            file.truncate(0)?;
            file.append(&header())?;
            file.sync()?;
            return Ok((Self::new(file), Vec::new()));
        }
        check_header(&bytes)?;
        let (records, len) = record::decode_all(&bytes[HEADER_LEN..])?;
        let valid_len = HEADER_LEN + len;
        if valid_len < bytes.len() {
            file.truncate(valid_len as u64)?;
            file.sync()?;
        }
        Ok((Self::new(file), records))
    }

    fn new(file: F) -> Self {
        Self {
            file,
            failed: false,
        }
    }

    /// Append `record` and sync it. Once this returns `Ok`, the Record
    /// survives a crash.
    ///
    /// If the append or sync fails, every later call returns
    /// [`Error::Failed`]: a partly written Record may be in the file, and a
    /// failed sync must not be retried (ADR 0010).
    pub fn append(&mut self, record: &Record) -> Result<()> {
        if self.failed {
            return Err(Error::Failed);
        }
        // Encoding fails before anything is written, so it leaves the WAL
        // usable.
        let bytes = record::encode(record)?;
        let written = self.file.append(&bytes).and_then(|()| self.file.sync());
        if written.is_err() {
            self.failed = true;
        }
        Ok(written?)
    }
}

fn header() -> Vec<u8> {
    let mut header = MAGIC.to_vec();
    header.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
    header
}

fn check_header(bytes: &[u8]) -> Result<()> {
    if bytes.len() < HEADER_LEN || &bytes[..4] != MAGIC {
        return Err(Error::Corruption("not a PalimpsestDB WAL".into()));
    }
    let version = u32::from_be_bytes(bytes[4..HEADER_LEN].try_into().expect("4 bytes"));
    if version != FORMAT_VERSION {
        return Err(Error::Corruption(format!(
            "unsupported WAL format version {version}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn record(commit_ts: u64) -> Record {
        Record {
            commit_ts,
            entries: vec![
                Entry {
                    key: b"put".to_vec(),
                    value: Some(commit_ts.to_be_bytes().to_vec()),
                },
                Entry {
                    key: b"deleted".to_vec(),
                    value: None,
                },
            ],
        }
    }

    fn open(file: &FaultyLogFile) -> Result<(Wal<FaultyLogFile>, Vec<Record>)> {
        Wal::open(file.clone())
    }

    /// The whole file: header, then `records` framed back to back.
    fn file_with(records: &[Record]) -> FaultyLogFile {
        let mut file = FaultyLogFile::new();
        file.append(&header()).unwrap();
        for r in records {
            file.append(&record::encode(r).unwrap()).unwrap();
        }
        file.sync().unwrap();
        file
    }

    #[test]
    fn new_file_gets_a_header_and_no_records() {
        let file = FaultyLogFile::new();
        let (_, records) = open(&file).unwrap();
        assert!(records.is_empty());
        assert_eq!(file.len(), HEADER_LEN);
        assert_eq!(file.unsynced_len(), 0);
    }

    #[test]
    fn appended_records_survive_a_crash() {
        let file = FaultyLogFile::new();
        let (mut wal, _) = open(&file).unwrap();
        wal.append(&record(1)).unwrap();
        wal.append(&record(2)).unwrap();
        file.crash(0);

        let (_, records) = open(&file).unwrap();
        assert_eq!(records, vec![record(1), record(2)]);
    }

    #[test]
    fn torn_header_is_rewritten() {
        let mut file = FaultyLogFile::new();
        file.append(b"PW").unwrap();
        let (_, records) = open(&file).unwrap();
        assert!(records.is_empty());
        assert_eq!(file.len(), HEADER_LEN);
    }

    #[test]
    fn foreign_file_is_rejected() {
        let mut file = FaultyLogFile::new();
        file.append(b"not a WAL").unwrap();
        assert!(matches!(open(&file), Err(Error::Corruption(_))));
    }

    #[test]
    fn other_format_version_is_rejected() {
        let mut file = FaultyLogFile::new();
        file.append(MAGIC).unwrap();
        file.append(&2u32.to_be_bytes()).unwrap();
        assert!(matches!(open(&file), Err(Error::Corruption(_))));
    }

    #[test]
    fn malformed_record_with_valid_checksum_is_corruption() {
        let payload = [0u8; 3];
        let mut file = file_with(&[]);
        file.append(&(payload.len() as u32).to_be_bytes()).unwrap();
        file.append(&crc32c::crc32c(&payload).to_be_bytes())
            .unwrap();
        file.append(&payload).unwrap();
        assert!(matches!(open(&file), Err(Error::Corruption(_))));
    }

    #[test]
    fn failed_sync_fails_every_later_append() {
        let file = FaultyLogFile::new();
        let (mut wal, _) = open(&file).unwrap();
        wal.append(&record(1)).unwrap();
        file.fail_next_sync();
        assert!(matches!(wal.append(&record(2)), Err(Error::Io(_))));
        assert!(matches!(wal.append(&record(3)), Err(Error::Failed)));
    }

    #[test]
    fn half_written_record_is_cut_off_on_reopen() {
        let file = FaultyLogFile::new();
        let (mut wal, _) = open(&file).unwrap();
        wal.append(&record(1)).unwrap();
        file.fail_next_append();
        assert!(wal.append(&record(2)).is_err());
        let torn_len = file.len();

        let (mut wal, records) = open(&file).unwrap();
        assert_eq!(records, vec![record(1)]);
        assert!(file.len() < torn_len);
        wal.append(&record(2)).unwrap();
        let (_, records) = open(&file).unwrap();
        assert_eq!(records, vec![record(1), record(2)]);
    }

    #[test]
    fn real_file_keeps_records_across_reopen() {
        let dir = std::env::temp_dir().join(format!("psdb-wal-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wal.log");
        let _ = std::fs::remove_file(&path);

        let (mut wal, _) = Wal::open(FsLogFile::open(&path).unwrap()).unwrap();
        wal.append(&record(1)).unwrap();
        wal.append(&record(2)).unwrap();
        drop(wal);
        let (_, records) = Wal::open(FsLogFile::open(&path).unwrap()).unwrap();
        assert_eq!(records, vec![record(1), record(2)]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn any_record() -> impl Strategy<Value = Record> {
        let entry = (
            prop::collection::vec(any::<u8>(), 0..16),
            prop::option::of(prop::collection::vec(any::<u8>(), 0..32)),
        )
            .prop_map(|(key, value)| Entry { key, value });
        (any::<u64>(), prop::collection::vec(entry, 0..5))
            .prop_map(|(commit_ts, entries)| Record { commit_ts, entries })
    }

    /// Where each Record ends in a file holding `records`.
    fn ends(records: &[Record]) -> Vec<usize> {
        records
            .iter()
            .scan(HEADER_LEN, |end, r| {
                *end += record::encode(r).unwrap().len();
                Some(*end)
            })
            .collect()
    }

    proptest! {
        #[test]
        fn encoding_round_trips(r in any_record()) {
            let bytes = record::encode(&r).unwrap();
            let (records, len) = record::decode_all(&bytes).unwrap();
            prop_assert_eq!(records, vec![r]);
            prop_assert_eq!(len, bytes.len());
        }

        #[test]
        fn cut_tail_leaves_the_records_before_it(
            records in prop::collection::vec(any_record(), 0..6),
            cut in any::<prop::sample::Index>(),
        ) {
            let mut file = file_with(&records);
            let cut = HEADER_LEN + cut.index(file.len() - HEADER_LEN + 1);
            file.truncate(cut as u64).unwrap();

            let intact = ends(&records).iter().filter(|&&end| end <= cut).count();
            let (_, recovered) = open(&file).unwrap();
            prop_assert_eq!(&recovered[..], &records[..intact]);
        }

        #[test]
        fn corrupt_byte_drops_its_record_and_everything_after(
            records in prop::collection::vec(any_record(), 1..6),
            at in any::<prop::sample::Index>(),
        ) {
            let file = file_with(&records);
            let at = HEADER_LEN + at.index(file.len() - HEADER_LEN);
            file.corrupt(at);

            let intact = ends(&records).iter().filter(|&&end| end <= at).count();
            let (mut wal, recovered) = open(&file).unwrap();
            prop_assert_eq!(&recovered[..], &records[..intact]);

            // The file now ends after the intact Records, so new ones follow
            // them.
            wal.append(&record(u64::MAX)).unwrap();
            let (_, reopened) = open(&file).unwrap();
            prop_assert_eq!(reopened.last(), Some(&record(u64::MAX)));
            prop_assert_eq!(reopened.len(), intact + 1);
        }
    }
}
