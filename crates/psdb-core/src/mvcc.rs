//! Version visibility on top of a Storage Engine (ADR 0001).
//!
//! Each Version is stored at its physical key (see [`crate::keys`]) with a
//! value that is either a tombstone or BSON bytes. A Snapshot at `ts` sees,
//! for each logical key, the newest Version committed at or before `ts`.

use std::ops::ControlFlow;

use psdb_storage::{KeyRange, StorageEngine};

use crate::error::{Error, Result};
use crate::keys::{physical_key, split_physical};

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

fn decode_value(stored: &[u8]) -> Result<Option<Vec<u8>>> {
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
    let mut found = None;
    engine.scan(&range, &mut |_, value| {
        found = Some(value.to_vec());
        ControlFlow::Break(())
    })?;
    match found {
        Some(stored) => decode_value(&stored),
        None => Ok(None),
    }
}

/// The Commit Timestamp of the newest Version of `logical`, whatever the
/// Snapshot, or `None` if it has never been written.
pub(crate) fn latest_commit_ts<E: StorageEngine>(
    engine: &E,
    logical: &[u8],
) -> Result<Option<u64>> {
    let mut found = None;
    engine.scan(&KeyRange::prefix(logical), &mut |key, _| {
        found = Some(key.to_vec());
        ControlFlow::Break(())
    })?;
    match found {
        Some(key) => Ok(Some(split_physical(&key)?.1)),
        None => Ok(None),
    }
}

/// Call `visit` with every logical key under `prefix` that has a live
/// Version at `snapshot_ts`, in key order, with that Version's value.
pub(crate) fn scan_at<E: StorageEngine>(
    engine: &E,
    prefix: &[u8],
    snapshot_ts: u64,
    visit: &mut dyn FnMut(&[u8], Vec<u8>),
) -> Result<()> {
    // The logical key whose visible Version was already found; its older
    // Versions are skipped.
    let mut decided: Option<Vec<u8>> = None;
    let mut failure = None;
    engine.scan(&KeyRange::prefix(prefix), &mut |key, value| {
        let (logical, ts) = match split_physical(key) {
            Ok(parts) => parts,
            Err(e) => {
                failure = Some(e);
                return ControlFlow::Break(());
            }
        };
        if ts > snapshot_ts || decided.as_deref() == Some(logical) {
            return ControlFlow::Continue(());
        }
        decided = Some(logical.to_vec());
        match decode_value(value) {
            Ok(Some(bytes)) => visit(logical, bytes),
            Ok(None) => {}
            Err(e) => {
                failure = Some(e);
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    })?;
    failure.map_or(Ok(()), Err)
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
    fn scan_at_returns_one_visible_version_per_key() {
        let engine = MemoryEngine::new();
        // Keys are self-delimiting in real use; here the 1-byte suffix stands in.
        put_version(&engine, b"pa", 1, Some(b"a1"));
        put_version(&engine, b"pa", 3, Some(b"a3"));
        put_version(&engine, b"pb", 2, Some(b"b2"));
        put_version(&engine, b"pb", 3, None);
        put_version(&engine, b"pc", 4, Some(b"c4"));
        put_version(&engine, b"q", 1, Some(b"outside prefix"));

        let collect = |ts| {
            let mut out = Vec::new();
            scan_at(&engine, b"p", ts, &mut |k, v| out.push((k.to_vec(), v))).unwrap();
            out
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
}
