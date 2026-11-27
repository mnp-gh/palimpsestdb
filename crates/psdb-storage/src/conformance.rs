//! Conformance suite for [`StorageEngine`] implementations.
//!
//! Every engine crate calls [`run_all`] from its tests with a function that
//! builds a fresh, empty engine:
//!
//! ```
//! # use psdb_storage::{conformance, StorageEngine};
//! # fn check<E: StorageEngine>(make_engine: impl Fn() -> E) {
//! conformance::run_all(make_engine);
//! # }
//! ```
//!
//! The suite checks the in-process rules of the contract (byte order, batch
//! atomicity and ordering, read-your-writes). Crash behaviour needs a
//! persistent engine and is checked by the crash tests that arrive with the
//! WAL.
#![expect(clippy::unwrap_used, reason = "test code: a failure should panic")]

use std::collections::BTreeMap;

use crate::{KeyRange, StorageEngine, WriteBatch, WriteOp};

/// Run every conformance check against fresh engines from `make`. Panics on
/// the first violation.
pub fn run_all<E: StorageEngine>(make: impl Fn() -> E) {
    get_missing_key(&make());
    put_then_get(&make());
    later_batch_overwrites(&make());
    delete_removes(&make());
    later_op_in_batch_wins(&make());
    empty_key_and_empty_value(&make());
    scan_is_byte_ordered(&make());
    scan_respects_range_bounds(&make());
    scan_prefix(&make());
    scan_survives_writes_between_next_calls(&make());
    matches_model_under_random_ops(&make());
}

fn get_missing_key<E: StorageEngine>(engine: &E) {
    assert_eq!(
        engine.get(b"missing").unwrap(),
        None,
        "a fresh engine must be empty"
    );
}

fn put_then_get<E: StorageEngine>(engine: &E) {
    write(engine, |b| {
        b.put("k", "v");
    });
    assert_eq!(engine.get(b"k").unwrap().as_deref(), Some(&b"v"[..]));
    engine.flush().expect("flush failed");
    assert_eq!(engine.get(b"k").unwrap().as_deref(), Some(&b"v"[..]));
}

fn write<E: StorageEngine>(engine: &E, build: impl FnOnce(&mut WriteBatch)) {
    let mut batch = WriteBatch::new();
    build(&mut batch);
    engine.write_batch(batch).expect("write_batch failed");
}

fn later_batch_overwrites<E: StorageEngine>(engine: &E) {
    write(engine, |b| {
        b.put("k", "v1");
    });
    write(engine, |b| {
        b.put("k", "v2");
    });
    assert_eq!(engine.get(b"k").unwrap().as_deref(), Some(&b"v2"[..]));
}

fn delete_removes<E: StorageEngine>(engine: &E) {
    write(engine, |b| {
        b.put("k", "v");
    });
    write(engine, |b| {
        b.delete("k").delete("never-existed");
    });
    assert_eq!(engine.get(b"k").unwrap(), None);
}

fn later_op_in_batch_wins<E: StorageEngine>(engine: &E) {
    write(engine, |b| {
        b.put("a", "1").delete("a").put("b", "1").put("b", "2");
    });
    assert_eq!(
        engine.get(b"a").unwrap(),
        None,
        "delete after put in one batch must win"
    );
    assert_eq!(
        engine.get(b"b").unwrap().as_deref(),
        Some(&b"2"[..]),
        "second put must win"
    );
}

fn empty_key_and_empty_value<E: StorageEngine>(engine: &E) {
    write(engine, |b| {
        b.put(Vec::new(), "empty-key")
            .put("empty-value", Vec::new());
    });
    assert_eq!(engine.get(b"").unwrap().as_deref(), Some(&b"empty-key"[..]));
    assert_eq!(
        engine.get(b"empty-value").unwrap().as_deref(),
        Some(&b""[..])
    );
}

fn scan_is_byte_ordered<E: StorageEngine>(engine: &E) {
    // Inserted out of order; includes 0x00, 0xFF and a key that is a prefix
    // of another, which must sort first.
    let keys: [&[u8]; 6] = [b"\xff", b"b", b"a\x00", b"a", b"\x00", b"ab"];
    write(engine, |b| {
        for k in keys {
            b.put(k, "v");
        }
    });
    let got = scanned_keys(engine, &KeyRange::all());
    let mut want: Vec<Vec<u8>> = keys.iter().map(|k| k.to_vec()).collect();
    want.sort();
    assert_eq!(got, want, "scan must visit keys in unsigned byte order");
}

/// The keys `scan` visits in `range`.
fn scanned_keys<E: StorageEngine>(engine: &E, range: &KeyRange) -> Vec<Vec<u8>> {
    collect(engine, range).into_iter().map(|(k, _)| k).collect()
}

fn collect<E: StorageEngine>(engine: &E, range: &KeyRange) -> Vec<(Vec<u8>, Vec<u8>)> {
    engine
        .scan(range)
        .collect::<crate::Result<_>>()
        .expect("scan failed")
}

fn scan_respects_range_bounds<E: StorageEngine>(engine: &E) {
    write(engine, |b| {
        b.put("a", "").put("b", "").put("c", "").put("d", "");
    });
    let got = scanned_keys(engine, &KeyRange::new("b", "d"));
    assert_eq!(
        got,
        vec![b"b".to_vec(), b"c".to_vec()],
        "start is inclusive, end exclusive"
    );
    assert_eq!(collect(engine, &KeyRange::new("x", "z")), []);
    for empty in [KeyRange::new("d", "b"), KeyRange::new("b", "b")] {
        assert_eq!(
            collect(engine, &empty),
            [],
            "an empty range (end <= start) visits nothing: {empty:?}"
        );
    }
}

fn scan_prefix<E: StorageEngine>(engine: &E) {
    write(engine, |b| {
        b.put("ab", "")
            .put(&b"ab\xff"[..], "")
            .put("abc", "")
            .put("ac", "")
            .put("a", "");
    });
    let got = scanned_keys(engine, &KeyRange::prefix(b"ab"));
    assert_eq!(
        got,
        vec![b"ab".to_vec(), b"abc".to_vec(), b"ab\xff".to_vec()]
    );
}

fn scan_survives_writes_between_next_calls<E: StorageEngine>(engine: &E) {
    // Enough keys that an engine copying entries a few at a time refills
    // mid-scan. Even keys are never written during the scan; odd ones are.
    let key = |i: u16| i.to_be_bytes().to_vec();
    write(engine, |b| {
        for i in 0..300 {
            b.put(key(i), key(i));
        }
    });
    let mut seen = Vec::new();
    for item in engine.scan(&KeyRange::all()) {
        let (k, v) = item.expect("scan failed");
        if let Ok(i) = <[u8; 2]>::try_from(k.as_slice()).map(u16::from_be_bytes) {
            write(engine, |b| {
                // Behind and ahead of the cursor, plus a key that did not exist.
                b.put(key(i.saturating_sub(9) | 1), "new")
                    .delete(key((i + 9) | 1))
                    .put([&k[..], b"\0"].concat(), "inserted");
            });
        }
        seen.push((k, v));
    }
    assert!(
        seen.windows(2).all(|w| w[0].0 < w[1].0),
        "scan must yield keys in ascending order, each once"
    );
    let untouched: Vec<_> = seen
        .into_iter()
        .filter(|(k, _)| k.len() == 2 && k[1] % 2 == 0)
        .collect();
    let want: Vec<_> = (0..300).step_by(2).map(|i| (key(i), key(i))).collect();
    assert_eq!(
        untouched, want,
        "a key not written during the scan comes exactly once, with its value"
    );
}

fn matches_model_under_random_ops<E: StorageEngine>(engine: &E) {
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

    for round in 0..500 {
        let mut batch = WriteBatch::new();
        for _ in 0..rng.below(5) {
            let key = rng.key();
            if rng.below(3) == 0 {
                batch.delete(key);
            } else {
                batch.put(key, round.to_string());
            }
        }
        for op in batch.ops() {
            match op {
                WriteOp::Put { key, value } => model.insert(key.clone(), value.clone()),
                WriteOp::Delete { key } => model.remove(key),
            };
        }
        engine.write_batch(batch).unwrap();

        let probe = rng.key();
        assert_eq!(
            engine.get(&probe).unwrap().as_ref(),
            model.get(&probe),
            "get({probe:?})"
        );

        let (a, b) = (rng.key(), rng.key());
        let range = KeyRange::new(a.clone().min(b.clone()), a.max(b));
        let want: Vec<(Vec<u8>, Vec<u8>)> = model
            .iter()
            .filter(|(k, _)| range.contains(k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        assert_eq!(
            collect(engine, &range),
            want,
            "scan({range:?}) in round {round}"
        );
    }
}

/// A small deterministic PRNG so the suite needs no dependencies.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).expect("less than n, so it fits")
    }

    fn key(&mut self) -> Vec<u8> {
        // A tiny alphabet with the edge bytes makes collisions and prefixes common.
        const ALPHABET: [u8; 4] = [0x00, b'a', b'b', 0xFF];
        let len = self.below(4);
        (0..len).map(|_| ALPHABET[self.below(4)]).collect()
    }
}
