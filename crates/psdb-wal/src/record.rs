//! WAL Record encoding (ADR 0010). Every integer is big-endian.

use crate::{Error, Result};

/// One committed Transaction: its Commit Timestamp and Write Set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The Transaction's Commit Timestamp.
    pub commit_ts: u64,
    /// Every key the Transaction wrote.
    pub entries: Vec<Entry>,
}

/// One write in a [`Record`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The logical key written, not its physical key.
    pub key: Vec<u8>,
    /// The value written, or `None` for a tombstone.
    pub value: Option<Vec<u8>>,
}

const TOMBSTONE: u8 = 0x00;
const PUT: u8 = 0x01;

/// `len (u32) ‖ crc (u32)`, before each payload.
const FRAME_HEADER_LEN: usize = 8;

/// Encode `record` as a framed Record: length, CRC32C, then payload.
pub(crate) fn encode(record: &Record) -> Result<Vec<u8>> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&record.commit_ts.to_be_bytes());
    push_len(&mut payload, record.entries.len())?;
    for entry in &record.entries {
        push_len(&mut payload, entry.key.len())?;
        payload.extend_from_slice(&entry.key);
        match &entry.value {
            None => payload.push(TOMBSTONE),
            Some(value) => {
                payload.push(PUT);
                push_len(&mut payload, value.len())?;
                payload.extend_from_slice(value);
            }
        }
    }
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    push_len(&mut out, payload.len())?;
    out.extend_from_slice(&crc32c::crc32c(&payload).to_be_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

fn push_len(out: &mut Vec<u8>, len: usize) -> Result<()> {
    let len = u32::try_from(len).map_err(|_| Error::RecordTooLarge)?;
    out.extend_from_slice(&len.to_be_bytes());
    Ok(())
}

/// Decode the Records in `bytes`, stopping at the first one that is cut
/// short or fails its checksum. Returns the Records and how many bytes of
/// `bytes` they span.
pub(crate) fn decode_all(bytes: &[u8]) -> Result<(Vec<Record>, usize)> {
    let mut records = Vec::new();
    let mut pos = 0;
    while let Some((payload, next)) = frame_at(bytes, pos) {
        records.push(decode_payload(payload)?);
        pos = next;
    }
    Ok((records, pos))
}

/// The payload of the Record starting at `pos`, and where the next Record
/// starts, or `None` if this one is torn.
fn frame_at(bytes: &[u8], pos: usize) -> Option<(&[u8], usize)> {
    let header = bytes.get(pos..pos + FRAME_HEADER_LEN)?;
    let (len, crc) = header.split_at(4);
    let len = u32::from_be_bytes(len.try_into().expect("4 bytes")) as usize;
    let crc = u32::from_be_bytes(crc.try_into().expect("4 bytes"));
    let start = pos + FRAME_HEADER_LEN;
    let payload = bytes.get(start..start + len)?;
    (crc32c::crc32c(payload) == crc).then_some((payload, start + len))
}

/// Decode a payload that passed its checksum. Anything malformed now is not
/// a torn write but a bug or a foreign format, so it is an error.
fn decode_payload(payload: &[u8]) -> Result<Record> {
    let mut reader = Reader { rest: payload };
    let commit_ts = reader.u64()?;
    let count = reader.u32()?;
    let mut entries = Vec::new();
    for _ in 0..count {
        let key_len = reader.u32()?;
        let key = reader.take(key_len)?.to_vec();
        let value = match reader.u8()? {
            TOMBSTONE => None,
            PUT => {
                let value_len = reader.u32()?;
                Some(reader.take(value_len)?.to_vec())
            }
            tag => return Err(malformed(&format!("unknown entry tag {tag:#04x}"))),
        };
        entries.push(Entry { key, value });
    }
    if !reader.rest.is_empty() {
        return Err(malformed("trailing bytes"));
    }
    Ok(Record { commit_ts, entries })
}

fn malformed(what: &str) -> Error {
    Error::Corruption(format!(
        "a WAL Record passed its checksum but is malformed: {what}"
    ))
}

/// A cursor over a payload.
struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.rest.len() {
            return Err(malformed("cut short"));
        }
        let (head, rest) = self.rest.split_at(n);
        self.rest = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<usize> {
        let bytes = self.take(4)?.try_into().expect("4 bytes");
        Ok(u32::from_be_bytes(bytes) as usize)
    }

    fn u64(&mut self) -> Result<u64> {
        let bytes = self.take(8)?.try_into().expect("8 bytes");
        Ok(u64::from_be_bytes(bytes))
    }
}
