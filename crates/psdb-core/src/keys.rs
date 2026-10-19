//! Order-preserving key encoding (ADR 0008).
//!
//! A **logical key** names one thing that has Versions: a Document, or a
//! Catalog entry. A **physical key** is a logical key plus an inverted
//! Commit Timestamp, so the newest Version of a key sorts first.

use bson::Bson;

use crate::error::{Error, Result};

/// The Database that holds the Catalog (ADR 0006).
pub(crate) const CATALOG_DATABASE: u64 = 0;
const COUNTER_COLLECTION: u64 = 0;
const DATABASES_COLLECTION: u64 = 1;
const COLLECTIONS_COLLECTION: u64 = 2;

/// The largest encoded Document ID accepted, in bytes.
pub const MAX_ID_BYTES: usize = 1024;

const TAG_INTEGER: u8 = 0x10;
const TAG_STRING: u8 = 0x20;
const TAG_BINARY: u8 = 0x30;
const TAG_OBJECT_ID: u8 = 0x40;
const TAG_BOOLEAN: u8 = 0x50;
const TAG_DATE_TIME: u8 = 0x60;

const TIMESTAMP_BYTES: usize = 8;

/// The key prefix shared by every logical key in one Collection.
pub(crate) fn collection_prefix(database_id: u64, collection_id: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(16);
    key.extend_from_slice(&database_id.to_be_bytes());
    key.extend_from_slice(&collection_id.to_be_bytes());
    key
}

/// The logical key of a Document.
pub(crate) fn document_key(database_id: u64, collection_id: u64, id: &Bson) -> Result<Vec<u8>> {
    let mut key = collection_prefix(database_id, collection_id);
    let start = key.len();
    encode_id(id, &mut key)?;
    if key.len() - start > MAX_ID_BYTES {
        return Err(Error::InvalidId(format!(
            "encoded _id exceeds {MAX_ID_BYTES} bytes"
        )));
    }
    Ok(key)
}

/// The logical key of the Catalog's ID counter.
pub(crate) fn counter_key() -> Vec<u8> {
    let mut key = collection_prefix(CATALOG_DATABASE, COUNTER_COLLECTION);
    push_escaped(&mut key, b"next_id");
    key
}

/// The prefix of every Database entry in the Catalog.
pub(crate) fn databases_prefix() -> Vec<u8> {
    collection_prefix(CATALOG_DATABASE, DATABASES_COLLECTION)
}

/// The logical key of a Database entry in the Catalog.
pub(crate) fn database_entry_key(name: &str) -> Vec<u8> {
    let mut key = databases_prefix();
    push_escaped(&mut key, name.as_bytes());
    key
}

/// The prefix of every Collection entry of one Database in the Catalog.
pub(crate) fn collections_prefix(database_id: u64) -> Vec<u8> {
    let mut key = collection_prefix(CATALOG_DATABASE, COLLECTIONS_COLLECTION);
    push_integer(&mut key, database_id as i64);
    key
}

/// The logical key of a Collection entry in the Catalog.
pub(crate) fn collection_entry_key(database_id: u64, name: &str) -> Vec<u8> {
    let mut key = collections_prefix(database_id);
    push_escaped(&mut key, name.as_bytes());
    key
}

/// Append the encoding of a Document ID. Rejects types that are not valid
/// Document IDs.
pub(crate) fn encode_id(id: &Bson, out: &mut Vec<u8>) -> Result<()> {
    match id {
        Bson::Int32(n) => push_integer(out, i64::from(*n)),
        Bson::Int64(n) => push_integer(out, *n),
        Bson::String(s) => {
            out.push(TAG_STRING);
            push_escaped(out, s.as_bytes());
        }
        Bson::Binary(b) => {
            out.push(TAG_BINARY);
            out.push(u8::from(b.subtype));
            push_escaped(out, &b.bytes);
        }
        Bson::ObjectId(oid) => {
            out.push(TAG_OBJECT_ID);
            out.extend_from_slice(&oid.bytes());
        }
        Bson::Boolean(b) => {
            out.push(TAG_BOOLEAN);
            out.push(u8::from(*b));
        }
        Bson::DateTime(dt) => {
            out.push(TAG_DATE_TIME);
            out.extend_from_slice(&flip_sign(dt.timestamp_millis()));
        }
        other => {
            return Err(Error::InvalidId(format!(
                "{:?} cannot be an _id; use an integer, string, binary (such as a UUID), ObjectId, boolean or date",
                other.element_type()
            )));
        }
    }
    Ok(())
}

fn push_integer(out: &mut Vec<u8>, n: i64) {
    out.push(TAG_INTEGER);
    out.extend_from_slice(&flip_sign(n));
}

/// Big-endian bytes of `n` with the sign bit flipped, so negative numbers
/// sort before positive ones.
fn flip_sign(n: i64) -> [u8; 8] {
    ((n as u64) ^ (1 << 63)).to_be_bytes()
}

/// Append `bytes` with each `0x00` escaped as `0x00 0xFF`, then the
/// terminator `0x00 0x00`. The terminator sorts below any escaped byte, so a
/// shorter string sorts first and no encoding is a prefix of another.
fn push_escaped(out: &mut Vec<u8>, bytes: &[u8]) {
    for &b in bytes {
        out.push(b);
        if b == 0x00 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0x00, 0x00]);
}

/// The physical key of the Version of `logical` committed at `commit_ts`.
pub(crate) fn physical_key(logical: &[u8], commit_ts: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(logical.len() + TIMESTAMP_BYTES);
    key.extend_from_slice(logical);
    key.extend_from_slice(&(!commit_ts).to_be_bytes());
    key
}

/// Split a physical key into its logical key and Commit Timestamp.
pub(crate) fn split_physical(physical: &[u8]) -> Result<(&[u8], u64)> {
    let split = physical
        .len()
        .checked_sub(TIMESTAMP_BYTES)
        .ok_or_else(|| Error::Corruption("physical key shorter than a timestamp".into()))?;
    let (logical, ts) = physical.split_at(split);
    let ts: [u8; 8] = ts.try_into().expect("split at 8 bytes from the end");
    Ok((logical, !u64::from_be_bytes(ts)))
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use bson::{Binary, Bson, DateTime, oid::ObjectId, spec::BinarySubtype};
    use proptest::prelude::*;

    use super::*;

    fn encode(id: &Bson) -> Vec<u8> {
        let mut out = Vec::new();
        encode_id(id, &mut out).unwrap();
        out
    }

    /// The order ADR 0008 promises, written independently of the encoding.
    fn reference_cmp(a: &Bson, b: &Bson) -> Ordering {
        fn rank(v: &Bson) -> u8 {
            match v {
                Bson::Int32(_) | Bson::Int64(_) => 0,
                Bson::String(_) => 1,
                Bson::Binary(_) => 2,
                Bson::ObjectId(_) => 3,
                Bson::Boolean(_) => 4,
                Bson::DateTime(_) => 5,
                _ => unreachable!(),
            }
        }
        fn int(v: &Bson) -> i64 {
            match v {
                Bson::Int32(n) => i64::from(*n),
                Bson::Int64(n) => *n,
                _ => unreachable!(),
            }
        }
        match (a, b) {
            _ if rank(a) != rank(b) => rank(a).cmp(&rank(b)),
            (Bson::String(x), Bson::String(y)) => x.as_bytes().cmp(y.as_bytes()),
            (Bson::Binary(x), Bson::Binary(y)) => {
                (u8::from(x.subtype), &x.bytes).cmp(&(u8::from(y.subtype), &y.bytes))
            }
            (Bson::ObjectId(x), Bson::ObjectId(y)) => x.bytes().cmp(&y.bytes()),
            (Bson::Boolean(x), Bson::Boolean(y)) => x.cmp(y),
            (Bson::DateTime(x), Bson::DateTime(y)) => {
                x.timestamp_millis().cmp(&y.timestamp_millis())
            }
            _ => int(a).cmp(&int(b)),
        }
    }

    fn any_id() -> impl Strategy<Value = Bson> {
        // Small alphabets with 0x00 make escapes and shared prefixes common.
        let text = prop::collection::vec(prop::sample::select(vec!['\0', 'a', 'b', 'é']), 0..4)
            .prop_map(|cs| cs.into_iter().collect::<String>());
        let bytes = prop::collection::vec(prop::sample::select(vec![0u8, 1, 0xFF]), 0..4);
        prop_oneof![
            any::<i32>().prop_map(Bson::Int32),
            any::<i64>().prop_map(Bson::Int64),
            (-3i64..3).prop_map(Bson::Int64),
            text.prop_map(Bson::String),
            (prop::sample::select(vec![0u8, 4]), bytes).prop_map(|(st, bytes)| {
                Bson::Binary(Binary {
                    subtype: BinarySubtype::from(st),
                    bytes,
                })
            }),
            any::<[u8; 16]>().prop_map(|b| Bson::Binary(Binary {
                subtype: BinarySubtype::Uuid,
                bytes: b.to_vec(),
            })),
            any::<[u8; 12]>().prop_map(|b| Bson::ObjectId(ObjectId::from_bytes(b))),
            any::<bool>().prop_map(Bson::Boolean),
            any::<i64>().prop_map(|ms| Bson::DateTime(DateTime::from_millis(ms))),
        ]
    }

    proptest! {
        #[test]
        fn encoding_preserves_order(a in any_id(), b in any_id()) {
            prop_assert_eq!(encode(&a).cmp(&encode(&b)), reference_cmp(&a, &b));
        }

        #[test]
        fn encoding_is_prefix_free(a in any_id(), b in any_id()) {
            let (ea, eb) = (encode(&a), encode(&b));
            if ea != eb {
                prop_assert!(!eb.starts_with(&ea) && !ea.starts_with(&eb));
            }
        }

        #[test]
        fn versions_never_interleave(a in any_id(), b in any_id(), ta: u64, tb: u64) {
            let (ka, kb) = (document_key(1, 1, &a).unwrap(), document_key(1, 1, &b).unwrap());
            prop_assume!(ka != kb);
            // Every Version of the smaller key sorts before every Version of the larger.
            prop_assert_eq!(physical_key(&ka, ta).cmp(&physical_key(&kb, tb)), ka.cmp(&kb));
        }

        #[test]
        fn physical_key_round_trips(a in any_id(), ts: u64) {
            let logical = document_key(3, 9, &a).unwrap();
            let physical = physical_key(&logical, ts);
            prop_assert_eq!(split_physical(&physical).unwrap(), (logical.as_slice(), ts));
        }
    }

    #[test]
    fn newer_versions_sort_first() {
        let key = document_key(1, 1, &Bson::Int32(7)).unwrap();
        assert!(physical_key(&key, 10) < physical_key(&key, 9));
    }

    #[test]
    fn int32_and_int64_are_the_same_id() {
        assert_eq!(encode(&Bson::Int32(5)), encode(&Bson::Int64(5)));
    }

    #[test]
    fn rejects_non_id_types() {
        for id in [
            Bson::Double(1.0),
            Bson::Null,
            Bson::Array(vec![]),
            Bson::Document(Default::default()),
        ] {
            assert!(
                matches!(encode_id(&id, &mut Vec::new()), Err(Error::InvalidId(_))),
                "{id:?}"
            );
        }
    }

    #[test]
    fn rejects_oversized_id() {
        let id = Bson::String("x".repeat(MAX_ID_BYTES));
        assert!(matches!(document_key(1, 1, &id), Err(Error::InvalidId(_))));
    }
}
