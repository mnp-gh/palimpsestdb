//! Version visibility on top of a Storage Engine (ADR 0001).
//!
//! Each Version is stored at its physical key (see [`crate::keys`]) with a
//! value that is either a tombstone or BSON bytes. A Snapshot at `ts` sees,
//! for each logical key, the newest Version committed at or before `ts`.

use psdb_storage::{KeyRange, StorageEngine, WriteBatch};

use crate::error::{Error, Result};
use crate::keys::{applied_up_to_key, physical_key, split_physical};
use crate::transaction::Write;

const TOMBSTONE: u8 = 0x00;
const LIVE: u8 = 0x01;

/// Encode a Version's value: `None` is a tombstone.
pub(crate) fn encode_value(value: Option<&[u8]>) -> Vec<u8> {
    match value {
        None => vec![TOMBSTONE],
        Some(bytes) => {
            let mut out = Vec::with_capacity(bytes.len() + 1);
            out.push(LIVE);
            out.extend_from_slice(bytes);
            out
        }
    }
}

/// The batch that Applies a committed Write Set: one Version per logical key
/// at `commit_ts`, plus the `applied_up_to` marker (ADR 0004).
pub(crate) fn commit_batch<'a>(
    commit_ts: u64,
    writes: impl IntoIterator<Item = Write<'a>>,
) -> WriteBatch {
    let mut batch = WriteBatch::new();
    for (key, value) in writes {
        batch.put(physical_key(key, commit_ts), encode_value(value));
    }
    batch.put(applied_up_to_key(), commit_ts.to_be_bytes());
    batch
}

/// The Commit Timestamp of the newest commit Applied to `engine`: 0 for an
/// empty engine, Corruption for one that holds data without it.
pub(crate) fn applied_up_to<E: StorageEngine>(engine: &E) -> Result<u64> {
    match engine.get(&applied_up_to_key())? {
        Some(bytes) => bytes
            .try_into()
            .map(u64::from_be_bytes)
            .map_err(|_| Error::Corruption("applied_up_to is not 8 bytes".into())),
        None if any_version_under(engine, &[])? => Err(Error::Corruption(
            "the storage engine holds data but no applied_up_to".into(),
        )),
        None => Ok(0),
    }
}

/// Whether a stored Version value is a tombstone.
pub(crate) fn is_tombstone(stored: &[u8]) -> bool {
    stored == [TOMBSTONE]
}

/// A stored Version value: `None` for a Tombstone.
pub(crate) fn decode_value(stored: &[u8]) -> Result<Option<Vec<u8>>> {
    match stored.split_first() {
        Some((&TOMBSTONE, [])) => Ok(None),
        Some((&LIVE, bytes)) => Ok(Some(bytes.to_vec())),
        _ => Err(Error::Corruption("unknown version value tag".into())),
    }
}

/// The value of `logical` visible at `snapshot_ts`, or `None` if it does
/// not exist or was deleted at that Snapshot.
pub(crate) fn read_at<E: StorageEngine>(
    engine: &E,
    logical: &[u8],
    snapshot_ts: u64,
) -> Result<Option<Vec<u8>>> {
    let mut range = KeyRange::prefix(logical);
    // Versions are newest first, so seeking to the Snapshot's timestamp lands
    // on the newest Version at or before it.
    range.start = physical_key(logical, snapshot_ts);
    match engine.scan(&range).next().transpose()? {
        Some((_, value)) => decode_value(&value),
        None => Ok(None),
    }
}

/// The Commit Timestamp of the newest Version of `logical`, whatever the
/// Snapshot, or `None` if it has never been written.
pub(crate) fn latest_commit_ts<E: StorageEngine>(
    engine: &E,
    logical: &[u8],
) -> Result<Option<u64>> {
    let Some((key, _)) = engine.scan(&KeyRange::prefix(logical)).next().transpose()? else {
        return Ok(None);
    };
    let (_, ts) = split_physical(&key)?;
    Ok(Some(ts))
}

/// Whether any Version of any key under `prefix` exists, live or deleted,
/// at any timestamp.
pub(crate) fn any_version_under<E: StorageEngine>(engine: &E, prefix: &[u8]) -> Result<bool> {
    Ok(engine.scan(&KeyRange::prefix(prefix)).next().transpose()?.is_some())
}

/// Every logical key in `range` that has a live Version at `snapshot_ts`,
/// in key order, with that Version's value. The range bounds compare
/// against physical keys, so a bound should be a logical key or a prefix of
/// one. The caller stops at the first error.
pub(crate) fn scan_range_at<'e, E: StorageEngine>(
    engine: &'e E,
    range: &KeyRange,
    snapshot_ts: u64,
) -> impl Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + use<'e, E> {
    // The logical key whose visible Version was already found; its older
    // Versions are skipped.
    let mut decided: Option<Vec<u8>> = None;
    engine
        .scan(range)
        .filter_map(move |version| visible(version, snapshot_ts, &mut decided).transpose())
}

/// The logical key and live value of `version` if it is the one a Snapshot
/// at `snapshot_ts` sees, given that the newest Versions of `decided` came
/// before it. `None` for a Version too new, an older Version of `decided`,
/// or a Tombstone.
fn visible(
    version: psdb_storage::Result<(Vec<u8>, Vec<u8>)>,
    snapshot_ts: u64,
    decided: &mut Option<Vec<u8>>,
) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
    let (key, value) = version?;
    let (logical, ts) = split_physical(&key)?;
    if ts > snapshot_ts || decided.as_deref() == Some(logical) {
        return Ok(None);
    }
    *decided = Some(logical.to_vec());
    Ok(decode_value(&value)?.map(|bytes| (logical.to_vec(), bytes)))
}

#[cfg(test)]
mod tests {
    use psdb_storage::{StorageEngine, WriteBatch};
    use psdb_storage_memory::MemoryEngine;

    use super::*;

    fn put_version(engine: &MemoryEngine, logical: &[u8], ts: u64, value: Option<&[u8]>) {
        let mut batch = WriteBatch::new();
        batch.put(physical_key(logical, ts), encode_value(value));
        engine.write_batch(batch).unwrap();
    }

    #[test]
    fn read_at_picks_newest_version_at_or_before_snapshot() {
        let engine = MemoryEngine::new();
        put_version(&engine, b"k", 5, Some(b"v5"));
        put_version(&engine, b"k", 8, Some(b"v8"));
        put_version(&engine, b"k", 12, None);

        assert_eq!(
            read_at(&engine, b"k", 4).unwrap(),
            None,
            "before the first Version"
        );
        assert_eq!(
            read_at(&engine, b"k", 5).unwrap().as_deref(),
            Some(&b"v5"[..])
        );
        assert_eq!(
            read_at(&engine, b"k", 7).unwrap().as_deref(),
            Some(&b"v5"[..])
        );
        assert_eq!(
            read_at(&engine, b"k", 11).unwrap().as_deref(),
            Some(&b"v8"[..])
        );
        assert_eq!(read_at(&engine, b"k", 12).unwrap(), None, "deleted at 12");
        assert_eq!(latest_commit_ts(&engine, b"k").unwrap(), Some(12));
        assert_eq!(latest_commit_ts(&engine, b"other").unwrap(), None);
    }

    #[test]
    fn scan_range_at_returns_one_visible_version_per_key() {
        let engine = MemoryEngine::new();
        // Keys are self-delimiting in real use; here the 1-byte suffix stands in.
        put_version(&engine, b"pa", 1, Some(b"a1"));
        put_version(&engine, b"pa", 3, Some(b"a3"));
        put_version(&engine, b"pb", 2, Some(b"b2"));
        put_version(&engine, b"pb", 3, None);
        put_version(&engine, b"pc", 4, Some(b"c4"));
        put_version(&engine, b"q", 1, Some(b"outside prefix"));

        let collect = |ts| {
            scan_range_at(&engine, &KeyRange::prefix(b"p"), ts)
                .collect::<Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(
            collect(2),
            vec![
                (b"pa".to_vec(), b"a1".to_vec()),
                (b"pb".to_vec(), b"b2".to_vec())
            ]
        );
        assert_eq!(
            collect(4),
            vec![
                (b"pa".to_vec(), b"a3".to_vec()),
                (b"pc".to_vec(), b"c4".to_vec())
            ]
        );
    }

    /// The value layout from ADR 0008, written out by hand.
    #[test]
    fn version_values_match_adr_0008_byte_for_byte() {
        assert_eq!(encode_value(None), [0x00]);
        assert_eq!(encode_value(Some(b"x")), [0x01, b'x']);
        assert_eq!(decode_value(&[0x00]).unwrap(), None);
        assert_eq!(decode_value(&[0x01, b'x']).unwrap(), Some(b"x".to_vec()));
        assert!(matches!(decode_value(&[0x02]), Err(Error::Corruption(_))));
    }
}
