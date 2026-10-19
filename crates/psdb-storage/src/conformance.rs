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

use std::collections::BTreeMap;
use std::ops::ControlFlow;

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
    scan_stops_on_break(&make());
    matches_model_under_random_ops(&make());
}

fn collect<E: StorageEngine>(engine: &E, range: &KeyRange) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    engine
        .scan(range, &mut |k, v| {
            out.push((k.to_vec(), v.to_vec()));
            ControlFlow::Continue(())
        })
        .expect("scan failed");
    out
}

fn write<E: StorageEngine>(engine: &E, build: impl FnOnce(&mut WriteBatch)) {
    let mut batch = WriteBatch::new();
    build(&mut batch);
    engine.write_batch(batch).expect("write_batch failed");
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
    let got: Vec<Vec<u8>> = collect(engine, &KeyRange::all())
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    let mut want: Vec<Vec<u8>> = keys.iter().map(|k| k.to_vec()).collect();
    want.sort();
    assert_eq!(got, want, "scan must visit keys in unsigned byte order");
}

fn scan_respects_range_bounds<E: StorageEngine>(engine: &E) {
    write(engine, |b| {
        b.put("a", "").put("b", "").put("c", "").put("d", "");
    });
    let got: Vec<Vec<u8>> = collect(engine, &KeyRange::new("b", "d"))
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        got,
        vec![b"b".to_vec(), b"c".to_vec()],
        "start is inclusive, end exclusive"
    );
    assert!(collect(engine, &KeyRange::new("x", "z")).is_empty());
}

fn scan_prefix<E: StorageEngine>(engine: &E) {
    write(engine, |b| {
        b.put("ab", "")
            .put(&b"ab\xff"[..], "")
            .put("abc", "")
            .put("ac", "")
            .put("a", "");
    });
    let got: Vec<Vec<u8>> = collect(engine, &KeyRange::prefix(b"ab"))
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        got,
        vec![b"ab".to_vec(), b"abc".to_vec(), b"ab\xff".to_vec()]
    );
}

fn scan_stops_on_break<E: StorageEngine>(engine: &E) {
    write(engine, |b| {
        b.put("a", "").put("b", "").put("c", "");
    });
    let mut seen = 0;
    engine
        .scan(&KeyRange::all(), &mut |_, _| {
            seen += 1;
            if seen == 2 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .unwrap();
    assert_eq!(seen, 2, "scan must stop when visit returns Break");
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

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn key(&mut self) -> Vec<u8> {
        // A tiny alphabet with the edge bytes makes collisions and prefixes common.
        const ALPHABET: [u8; 4] = [0x00, b'a', b'b', 0xFF];
        let len = self.below(4) as usize;
        (0..len).map(|_| ALPHABET[self.below(4) as usize]).collect()
    }
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
