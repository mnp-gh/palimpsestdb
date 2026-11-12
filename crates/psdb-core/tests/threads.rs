//! Multi-threaded Node, Snapshot registry, Horizon tracking, and thread-safety tests.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use bson::{Bson, doc};
use common::collection;
use psdb_core::{FieldSchema, FieldType, Node, Transaction};
use psdb_storage_memory::MemoryEngine;

#[test]
fn compile_time_send_sync_assertions() {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}

    assert_send::<Node<MemoryEngine>>();
    assert_sync::<Node<MemoryEngine>>();
    assert_send::<Transaction<'static, MemoryEngine>>();
}

#[test]
fn horizon_tracks_active_snapshots_and_advances_on_drop() {
    let node = common::shop();

    // Begin a reader; horizon stays pinned to reader's snapshot.
    let reader1 = node.begin_transaction();
    let snap1 = reader1.snapshot_ts();
    assert_eq!(node.horizon(), snap1);

    // Commit writes; latest_visible_commit_ts advances, but horizon remains at snap1.
    node.insert(&collection("shop.orders"), doc! { "item": "book" })
        .unwrap();
    assert_eq!(node.horizon(), snap1);

    // Begin a second reader at the new visible commit.
    let reader2 = node.begin_transaction();
    let snap2 = reader2.snapshot_ts();
    assert!(snap2 > snap1);
    assert_eq!(node.horizon(), snap1);

    // Dropping the first reader advances horizon to reader2's snapshot.
    drop(reader1);
    assert_eq!(node.horizon(), snap2);

    // Dropping reader2 advances horizon to latest_visible_commit_ts.
    drop(reader2);
    assert!(node.horizon() >= snap2);
}

#[test]
fn horizon_advances_when_transaction_aborts_or_panics() {
    let node = Arc::new(common::shop());

    // Explicit abort unregisters.
    let txn = node.begin_transaction();
    let snap = txn.snapshot_ts();
    assert_eq!(node.horizon(), snap);
    txn.abort();

    // Panic on a thread unregisters via Drop.
    let node_clone = Arc::clone(&node);
    let handle = thread::spawn(move || {
        let _txn = node_clone.begin_transaction();
        panic!("simulated worker thread panic");
    });
    let _ = handle.join();

    // Horizon should not be pinned by the panicked thread.
    node.insert(&collection("shop.orders"), doc! { "item": "pen" })
        .unwrap();
    let next_txn = node.begin_transaction();
    assert_eq!(node.horizon(), next_txn.snapshot_ts());
}

#[test]
fn begin_fails_fast_when_commit_lock_is_poisoned() {
    let node = common::shop();

    // Poison the commit lock by panicking while holding it.
    node.poison_commit_lock_for_test();

    // begin_transaction() must detect the poisoned commit lock and panic.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        node.begin_transaction();
    }));
    assert!(
        result.is_err(),
        "begin_transaction() must fail fast on poisoned commit lock"
    );
}

#[test]
fn concurrent_threads_stress_test() {
    let node = Arc::new(common::shop());
    let orders = collection("shop.orders");
    let running = Arc::new(AtomicBool::new(true));

    let mut handles = Vec::new();

    // 4 Writer threads
    for thread_id in 0..4 {
        let node = Arc::clone(&node);
        let orders = orders.clone();
        let running = Arc::clone(&running);
        handles.push(thread::spawn(move || {
            let mut i = 0;
            while running.load(Ordering::Relaxed) {
                i += 1;
                let id = Bson::String(format!("t{thread_id}_{i}"));
                node.insert(&orders, doc! { "_id": id, "count": i })
                    .unwrap();
                thread::yield_now();
            }
        }));
    }

    // 4 Reader threads
    for _ in 0..4 {
        let node = Arc::clone(&node);
        let orders = orders.clone();
        let running = Arc::clone(&running);
        handles.push(thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                let mut txn = node.begin_transaction();
                let snap = txn.snapshot_ts();
                // Horizon must never exceed any active transaction's snapshot.
                assert!(node.horizon() <= snap);
                txn.find(&orders, &doc! {}).unwrap();
                thread::yield_now();
            }
        }));
    }

    // Let threads run for 200ms
    thread::sleep(Duration::from_millis(200));
    running.store(false, Ordering::Relaxed);

    for h in handles {
        h.join().expect("thread panicked");
    }

    // After all threads exit, horizon equals latest visible.
    let h = node.horizon();
    let txn = node.begin_transaction();
    assert_eq!(h, txn.snapshot_ts());
}

#[test]
fn other_threads_commit_while_an_index_builds() {
    const DOCS: i32 = 10_000;
    let node = common::shop();
    let orders = collection("shop.orders");
    let n = common::field_path("n");
    node.run(|t| {
        for i in 0..DOCS {
            t.insert(&orders, doc! { "_id": i, "n": i % 100 })?;
        }
        Ok(())
    })
    .unwrap();
    node.set_field_type(
        &orders,
        &n,
        FieldSchema {
            field_type: FieldType::Int,
            strict: false,
        },
    )
    .unwrap();

    let building = AtomicBool::new(true);
    let committed_during_build = AtomicUsize::new(0);
    thread::scope(|s| {
        for writer in 0..4 {
            let (node, orders, building, committed) =
                (&node, &orders, &building, &committed_during_build);
            s.spawn(move || {
                let mut i = 0;
                while building.load(Ordering::Relaxed) {
                    i += 1;
                    // Spread over the whole Collection, ahead of the build
                    // and behind it; new `_id`s land after every chunk.
                    let id = (i * 7919 + writer * 31) % (DOCS + 500);
                    let outcome = node.run(|t| {
                        if i % 5 == 0 {
                            t.delete(orders, &Bson::Int32(id)).map(drop)
                        } else if t.replace(orders, &Bson::Int32(id), doc! { "n": i % 100 })? {
                            Ok(())
                        } else {
                            t.insert(orders, doc! { "_id": id, "n": i % 100 }).map(drop)
                        }
                    });
                    match outcome {
                        Ok(()) => {
                            committed.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) if e.is_retryable() => {}
                        Err(e) => panic!("{e}"),
                    }
                }
            });
        }
        node.create_index(&orders, &n, false).unwrap();
        building.store(false, Ordering::Relaxed);
    });
    assert!(
        committed_during_build.load(Ordering::Relaxed) > 0,
        "the build never let another commit through"
    );

    let all = node.find(&orders, &doc! {}).unwrap();
    let ids = |docs: &[bson::Document]| -> Vec<Bson> {
        docs.iter().map(|d| d.get("_id").unwrap().clone()).collect()
    };
    for (filter, values) in [
        (doc! { "n": 7 }, 7..=7),
        (doc! { "n": { "$gte": 90 } }, 90..=99),
    ] {
        let want: Vec<_> = all
            .iter()
            .filter(|d| values.contains(&d.get_i32("n").unwrap()))
            .cloned()
            .collect();
        assert_eq!(
            ids(&node.find(&orders, &filter).unwrap()),
            ids(&want),
            "{filter}"
        );
    }
}
