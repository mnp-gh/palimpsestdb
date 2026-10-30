//! The WAL: one record per committed Transaction, appended and fsynced
//! before the Transaction is Applied (ADR 0004).
//!
//! The file is a header followed by records:
//!
//! ```text
//! header := magic (8 bytes) ‖ format version (u32)
//! record := body length (u32) ‖ CRC32C of body (u32) ‖ body
//! body   := Commit Timestamp (u64) ‖ write count (u32) ‖ write*
//! write  := key length (u32) ‖ key ‖ 0x00                         (tombstone)
//!         | key length (u32) ‖ key ‖ 0x01 ‖ value length (u32) ‖ value
//! ```
//!
//! Integers are big-endian, like every integer PalimpsestDB encodes itself
//! (BSON, which is stored as is, has its own little-endian format). Keys
//! are logical keys and values are BSON, exactly as in the Write Set.
//!
//! All I/O goes through the [`WalFile`] seam, so tests can simulate power
//! loss with [`SimDisk`]. Every implementation passes [`conformance`].
//!
//! The seam is a `Box<dyn WalFile>` rather than a generic parameter, an
//! exception to the generics-at-seams rule in `docs/CONVENTIONS.md`: a
//! generic would spread a second type parameter through `Database` and
//! `Transaction` for a virtual call that costs nothing next to an fsync.
//! [`SimDisk`] is public, like `psdb_storage::conformance`, because the
//! crash tests in `tests/` can only use public items.

pub mod conformance;
mod file;
mod sim;

pub use file::{OsWalFile, WalFile};
pub use sim::{SimDisk, SimWalFile};

use crate::error::{Error, Result};
use crate::transaction::Write;

const MAGIC: [u8; 8] = *b"PSDBWAL\0";
const FORMAT_VERSION: u32 = 1;
const HEADER_LEN: usize = MAGIC.len() + 4;
/// Length and CRC32C.
const FRAME_LEN: usize = 8;
/// Commit Timestamp and write count.
const MIN_BODY_LEN: usize = 12;

// The WAL's own tags, independent of the Version value encoding in `mvcc`
// although the bytes happen to match.
const TOMBSTONE: u8 = 0x00;
const LIVE: u8 = 0x01;

/// One committed Transaction read back from the WAL.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) commit_ts: u64,
    /// Logical key → new value (`None` deletes), as in the Transaction.
    pub(crate) write_set: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

impl Record {
    /// The Write Set, in the order it was logged.
    pub(crate) fn writes(&self) -> impl Iterator<Item = Write<'_>> {
        self.write_set
            .iter()
            .map(|(k, v)| (k.as_slice(), v.as_deref()))
    }
}

/// An open WAL, positioned to append after its last valid record.
pub(crate) struct Wal {
    file: Box<dyn WalFile>,
}

impl Wal {
    /// Open the WAL in `file`, creating it if empty, and return every
    /// committed Transaction in it in Commit Timestamp order.
    ///
    /// - A file shorter than the header whose bytes start the expected
    ///   header was cut off while being created, before any commit, and is
    ///   reinitialised. Any other short file is refused.
    /// - A wrong magic number or format version is refused and the file is
    ///   left untouched.
    /// - A bad record with no valid record after it is a torn write and is
    ///   truncated. A bad record followed by a valid one is corruption, and
    ///   open refuses rather than drop acknowledged commits.
    /// - A record whose Commit Timestamp does not exceed the previous one is
    ///   corruption.
    pub(crate) fn open(mut file: Box<dyn WalFile>) -> Result<(Self, Vec<Record>)> {
        let bytes = file.read_all()?;
        let header = header();
        if bytes.len() < HEADER_LEN {
            if !header.starts_with(&bytes) {
                return Err(Error::UnrecognisedWal(format!(
                    "{} bytes long, shorter than a WAL header",
                    bytes.len()
                )));
            }
            file.truncate(0)?;
            file.append(&header)?;
            file.sync()?;
            file.sync_dir()?;
            return Ok((Self { file }, Vec::new()));
        }
        if bytes[..MAGIC.len()] != MAGIC {
            return Err(Error::UnrecognisedWal(
                "not a PalimpsestDB WAL (wrong magic number)".into(),
            ));
        }
        let version = read_u32(&bytes[MAGIC.len()..]);
        if version != FORMAT_VERSION {
            return Err(Error::UnrecognisedWal(format!(
                "format version {version}; this build reads version {FORMAT_VERSION}"
            )));
        }

        let mut records = Vec::new();
        let mut last_ts = 0;
        let mut pos = HEADER_LEN;
        while pos < bytes.len() {
            let Some(body) = body_at(&bytes, pos) else {
                // Every commit is synced before the next is appended, so a
                // torn write is always the last record. Any intact record
                // after this one, wherever it starts and whatever its
                // Commit Timestamp, means the damage is mid-log. Searching
                // every offset, rather than trusting this record's possibly
                // damaged length, prefers refusing to open over dropping a
                // Durable commit; a Document holding the bytes of a WAL
                // record inside a torn write can make it refuse needlessly.
                if (pos + 1..bytes.len()).any(|p| body_at(&bytes, p).is_some()) {
                    return Err(Error::Corruption(format!(
                        "bad WAL record at byte {pos} is followed by valid records"
                    )));
                }
                file.truncate(pos as u64)?;
                file.sync()?;
                break;
            };
            let record = decode_body(body)?;
            if record.commit_ts <= last_ts {
                return Err(Error::Corruption(format!(
                    "WAL record at byte {pos} has Commit Timestamp {}, not greater than the previous record's {last_ts}",
                    record.commit_ts
                )));
            }
            last_ts = record.commit_ts;
            records.push(record);
            pos += FRAME_LEN + body.len();
        }
        // The file may have been created by a run that crashed before
        // syncing its directory entry.
        file.sync_dir()?;
        Ok((Self { file }, records))
    }

    /// Append the record of a commit and fsync it. Once this returns `Ok`
    /// the commit is Durable.
    ///
    /// On error the record may or may not be on disk, and must not be
    /// retried (ADR 0004).
    pub(crate) fn append<'a>(
        &mut self,
        commit_ts: u64,
        writes: impl IntoIterator<Item = Write<'a>>,
    ) -> std::io::Result<()> {
        let record = encode_record(commit_ts, writes)?;
        self.file.append(&record)?;
        self.file.sync()
    }
}

fn header() -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
    out
}

fn encode_record<'a>(
    commit_ts: u64,
    writes: impl IntoIterator<Item = Write<'a>>,
) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    body.extend_from_slice(&commit_ts.to_be_bytes());
    body.extend_from_slice(&[0; 4]);
    let mut count: u32 = 0;
    for (key, value) in writes {
        push_bytes(&mut body, key)?;
        match value {
            None => body.push(TOMBSTONE),
            Some(value) => {
                body.push(LIVE);
                push_bytes(&mut body, value)?;
            }
        }
        count += 1;
    }
    body[8..12].copy_from_slice(&count.to_be_bytes());

    let mut out = Vec::with_capacity(FRAME_LEN + body.len());
    out.extend_from_slice(&len_u32(body.len())?.to_be_bytes());
    out.extend_from_slice(&crc32c::crc32c(&body).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

fn push_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> std::io::Result<()> {
    out.extend_from_slice(&len_u32(bytes.len())?.to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

fn len_u32(len: usize) -> std::io::Result<u32> {
    u32::try_from(len).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "WAL record larger than 4 GiB",
        )
    })
}

/// The body of the record at `pos` if its length fits in the file and its
/// CRC32C matches.
fn body_at(bytes: &[u8], pos: usize) -> Option<&[u8]> {
    let frame = bytes.get(pos..pos + FRAME_LEN)?;
    let len = read_u32(frame) as usize;
    let crc = read_u32(&frame[4..]);
    let body = bytes.get(pos + FRAME_LEN..pos + FRAME_LEN + len)?;
    (len >= MIN_BODY_LEN && crc32c::crc32c(body) == crc).then_some(body)
}

/// Decode a body whose CRC32C matched. A malformed one cannot be a torn
/// write, so it is corruption.
fn decode_body(body: &[u8]) -> Result<Record> {
    let malformed = || Error::Corruption("malformed WAL record".into());
    let mut reader = Reader { rest: body };
    let commit_ts = read_u64(reader.take(8).ok_or_else(malformed)?);
    let count = read_u32(reader.take(4).ok_or_else(malformed)?);
    let mut writes = Vec::new();
    for _ in 0..count {
        let key = reader.bytes().ok_or_else(malformed)?.to_vec();
        let value = match reader.take(1).ok_or_else(malformed)? {
            [TOMBSTONE] => None,
            [LIVE] => Some(reader.bytes().ok_or_else(malformed)?.to_vec()),
            _ => return Err(malformed()),
        };
        writes.push((key, value));
    }
    if !reader.rest.is_empty() {
        return Err(malformed());
    }
    Ok(Record {
        commit_ts,
        write_set: writes,
    })
}

struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.rest.len() < n {
            return None;
        }
        let (head, rest) = self.rest.split_at(n);
        self.rest = rest;
        Some(head)
    }

    /// A length-prefixed byte string.
    fn bytes(&mut self) -> Option<&'a [u8]> {
        let len = read_u32(self.take(4)?) as usize;
        self.take(len)
    }
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().expect("4 bytes"))
}

fn read_u64(bytes: &[u8]) -> u64 {
    u64::from_be_bytes(bytes[..8].try_into().expect("8 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(commit_ts: u64, writes: &[(&[u8], Option<&[u8]>)]) -> Vec<u8> {
        encode_record(commit_ts, writes.iter().copied()).unwrap()
    }

    /// A disk holding a WAL with `records` already written and synced.
    fn disk_with(records: &[Vec<u8>]) -> SimDisk {
        let disk = SimDisk::new();
        let mut bytes = header();
        for r in records {
            bytes.extend_from_slice(r);
        }
        disk.set_contents(bytes);
        disk
    }

    fn open(disk: &SimDisk) -> Result<Vec<Record>> {
        Wal::open(Box::new(disk.file())).map(|(_, records)| records)
    }

    #[test]
    fn records_round_trip() {
        let disk = disk_with(&[
            record(1, &[(b"a", Some(b"1")), (b"b", None)]),
            record(2, &[]),
        ]);
        assert_eq!(
            open(&disk).unwrap(),
            vec![
                Record {
                    commit_ts: 1,
                    write_set: vec![(b"a".to_vec(), Some(b"1".to_vec())), (b"b".to_vec(), None)],
                },
                Record {
                    commit_ts: 2,
                    write_set: vec![]
                },
            ]
        );
    }

    #[test]
    fn appended_records_are_read_back() {
        let disk = SimDisk::new();
        let (mut wal, records) = Wal::open(Box::new(disk.file())).unwrap();
        assert!(records.is_empty());
        wal.append(1, [(&b"k"[..], Some(&b"v"[..]))]).unwrap();
        wal.append(2, [(&b"k"[..], None)]).unwrap();
        drop(wal);
        assert_eq!(open(&disk).unwrap().len(), 2);
    }

    #[test]
    fn creates_header_and_syncs_directory() {
        let disk = SimDisk::new();
        open(&disk).unwrap();
        disk.power_loss(0);
        assert_eq!(disk.contents(), header(), "the file survives power loss");
    }

    #[test]
    fn torn_tail_is_truncated() {
        let good = record(1, &[(b"a", Some(b"1"))]);
        let torn = record(2, &[(b"b", Some(b"2"))]);
        for cut in 1..torn.len() {
            let disk = disk_with(&[good.clone(), torn[..cut].to_vec()]);
            assert_eq!(open(&disk).unwrap().len(), 1, "cut at {cut}");
            assert_eq!(
                disk.contents().len(),
                HEADER_LEN + good.len(),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn zeroed_tail_is_truncated() {
        let good = record(1, &[(b"a", Some(b"1"))]);
        let disk = disk_with(&[good.clone(), vec![0; 100]]);
        assert_eq!(open(&disk).unwrap().len(), 1);
        assert_eq!(disk.contents().len(), HEADER_LEN + good.len());
    }

    #[test]
    fn corruption_followed_by_valid_records_refuses_to_open() {
        let mut bad = record(1, &[(b"a", Some(b"1"))]);
        let last = bad.len() - 1;
        bad[last] ^= 0xFF;
        let disk = disk_with(&[bad, record(2, &[(b"b", Some(b"2"))])]);
        let before = disk.contents();
        assert!(matches!(open(&disk), Err(Error::Corruption(_))));
        assert_eq!(disk.contents(), before, "nothing truncated");
    }

    #[test]
    fn corruption_followed_by_an_older_record_refuses_to_open() {
        let mut bad = record(2, &[(b"a", Some(b"1"))]);
        let last = bad.len() - 1;
        bad[last] ^= 0xFF;
        let disk = disk_with(&[record(1, &[]), bad, record(1, &[])]);
        assert!(matches!(open(&disk), Err(Error::Corruption(_))));
    }

    #[test]
    fn non_increasing_commit_timestamp_is_corruption() {
        for second in [1, 0] {
            let disk = disk_with(&[record(1, &[]), record(second, &[])]);
            assert!(matches!(open(&disk), Err(Error::Corruption(_))), "{second}");
        }
    }

    #[test]
    fn wrong_magic_or_version_is_refused_and_left_untouched() {
        let mut wrong_magic = b"NOTAWAL!".to_vec();
        wrong_magic.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
        wrong_magic.extend_from_slice(b"someone else's data");
        let mut wrong_version = MAGIC.to_vec();
        wrong_version.extend_from_slice(&(FORMAT_VERSION + 1).to_be_bytes());
        for bytes in [wrong_magic, wrong_version] {
            let disk = SimDisk::new();
            disk.set_contents(bytes.clone());
            assert!(matches!(open(&disk), Err(Error::UnrecognisedWal(_))));
            assert_eq!(disk.contents(), bytes);
        }
    }

    #[test]
    fn partial_header_is_reinitialised() {
        for cut in 0..HEADER_LEN {
            let disk = SimDisk::new();
            disk.set_contents(header()[..cut].to_vec());
            assert!(open(&disk).unwrap().is_empty(), "cut at {cut}");
            assert_eq!(disk.contents(), header());
        }
    }

    #[test]
    fn other_short_file_is_refused() {
        let disk = SimDisk::new();
        disk.set_contents(b"hello".to_vec());
        assert!(matches!(open(&disk), Err(Error::UnrecognisedWal(_))));
        assert_eq!(disk.contents(), b"hello");
    }
}
