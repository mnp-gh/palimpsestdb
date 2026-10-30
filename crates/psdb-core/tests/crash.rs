//! Crash tests: crash at random points, recover, and check that
//!
//! 1. every acknowledged commit is present,
//! 2. every recovered commit was attempted, and
//! 3. the recovered commits are an unbroken run in Commit Timestamp order.
//!
//! A commit in flight at the crash may or may not survive. Two kinds of
//! crash: simulated power loss on a [`SimDisk`], which drops unsynced bytes,
//! and killing a real child process, which exercises the real filesystem.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use bson::{Bson, doc};
use proptest::prelude::*;
use psdb_core::wal::SimDisk;
use psdb_core::{Database, Limits, Transaction};
use psdb_storage_memory::MemoryEngine;

/// What a Database holds: whether `shop` exists, and `shop.orders` as
/// `_id → v` once it exists.
#[derive(Debug, Clone, PartialEq)]
struct State {
    shop: bool,
    orders: Option<BTreeMap<i32, i32>>,
}

/// One write in a Transaction: set `_id` to `v`, or delete it.
type Op = (i32, Option<i32>);

/// The `_id` every workload Transaction sets to its own sequence number, so
/// each commit leaves a different State and a recovered State names exactly
/// how many commits survived.
const SEQUENCE_ID: i32 = -1;

fn open(disk: &SimDisk) -> psdb_core::Result<Database<MemoryEngine>> {
    Database::open_with(MemoryEngine::new(), disk.file(), Limits::default())
}

fn state(db: &Database<MemoryEngine>) -> State {
    let shop = db.list_databases().unwrap().contains(&"shop".to_string());
    let has_orders = shop
        && db
            .list_collections("shop")
            .unwrap()
            .contains(&"orders".to_string());
    let orders = has_orders.then(|| {
        db.find("shop", "orders", &doc! {})
            .unwrap()
            .into_iter()
            .map(|d| (d.get_i32("_id").unwrap(), d.get_i32("v").unwrap()))
            .collect()
    });
    State { shop, orders }
}

/// Set `_id` to `v` in `shop.orders`, inserting or replacing.
fn upsert(t: &mut Transaction<'_, MemoryEngine>, id: i32, v: i32) -> psdb_core::Result<()> {
    if !t.replace("shop", "orders", &Bson::Int32(id), doc! { "v": v })? {
        t.insert("shop", "orders", doc! { "_id": id, "v": v })?;
    }
    Ok(())
}

/// Run `txn`, the `sequence`th workload Transaction, and commit it.
fn run(db: &Database<MemoryEngine>, sequence: i32, txn: &[Op]) -> psdb_core::Result<()> {
    let mut t = db.begin();
    upsert(&mut t, SEQUENCE_ID, sequence)?;
    for &(id, value) in txn {
        match value {
            Some(v) => upsert(&mut t, id, v)?,
            None => {
                t.delete("shop", "orders", &Bson::Int32(id))?;
            }
        }
    }
    t.commit().map(|_| ())
}

fn transaction() -> impl Strategy<Value = Vec<Op>> {
    let write = (0..6i32, prop::option::of(0..100i32));
    prop::collection::vec(write, 1..4)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn power_loss_keeps_exactly_a_prefix_of_commits(
        txns in prop::collection::vec(transaction(), 0..20),
        crash_after in 0..80usize,
        keep_unsynced in prop_oneof![Just(0), Just(usize::MAX), 0..200usize],
    ) {
        // states[n] is the expected State after the first n commits; no two
        // are equal.
        let mut states = vec![
            State { shop: false, orders: None },
            State { shop: true, orders: None },
            State { shop: true, orders: Some(BTreeMap::new()) },
        ];
        for (sequence, txn) in (0..).zip(&txns) {
            let mut orders = states.last().unwrap().orders.clone().unwrap();
            orders.insert(SEQUENCE_ID, sequence);
            for &(id, value) in txn {
                match value {
                    Some(v) => orders.insert(id, v),
                    None => orders.remove(&id),
                };
            }
            states.push(State { shop: true, orders: Some(orders) });
        }

        let disk = SimDisk::new();
        disk.crash_after(crash_after, keep_unsynced);
        let mut acknowledged = 0;
        if let Ok(db) = open(&disk) {
            let setup: [&dyn Fn() -> psdb_core::Result<()>; 2] = [
                &|| db.create_database("shop"),
                &|| db.create_collection("shop", "orders"),
            ];
            let commits = setup
                .iter()
                .map(|f| f())
                .chain((0..).zip(&txns).map(|(sequence, t)| run(&db, sequence, t)));
            for outcome in commits {
                if outcome.is_err() {
                    break;
                }
                acknowledged += 1;
            }
        }
        // If the scheduled crash never fired, cut the power now.
        disk.power_loss(keep_unsynced);

        // Since every State differs, matching one proves the recovered
        // commits are exactly the first `survived`: none lost, none invented,
        // no gap.
        let recovered = state(&open(&disk).unwrap());
        let survived = states.iter().position(|s| *s == recovered);
        prop_assert!(
            survived == Some(acknowledged) || survived == Some(acknowledged + 1),
            "recovered {recovered:?} (commit {survived:?}) after {acknowledged} acknowledged"
        );

        // The recovered Database keeps working and survives another reopen.
        let db = open(&disk).unwrap();
        prop_assert_eq!(state(&db), recovered.clone());
        if recovered.orders.is_some() {
            db.insert("shop", "orders", doc! { "_id": 99, "v": 0 }).unwrap();
            drop(db);
            let orders = state(&open(&disk).unwrap()).orders.unwrap();
            prop_assert_eq!(orders.get(&99).copied(), Some(0));
        }
    }
}

/// Set in the child process, to the WAL directory it writes to.
const CHILD_DIR: &str = "PSDB_CRASH_CHILD_DIR";

/// The child side of [`kill_keeps_exactly_a_prefix_of_commits`]: insert
/// `_id`s 0, 1, 2, … until killed, printing each acknowledged one. Does
/// nothing when run as an ordinary test.
#[test]
fn kill_child() {
    let Some(dir) = std::env::var_os(CHILD_DIR) else {
        return;
    };
    let db = Database::open(MemoryEngine::new(), dir).unwrap();
    db.create_database("shop").unwrap();
    db.create_collection("shop", "orders").unwrap();
    let mut out = std::io::stdout().lock();
    for id in 0.. {
        db.insert("shop", "orders", doc! { "_id": id, "v": 0 })
            .unwrap();
        writeln!(out, "acknowledged {id}").unwrap();
        out.flush().unwrap();
    }
}

#[test]
fn kill_keeps_exactly_a_prefix_of_commits() {
    for round in 0..5 {
        let dir = std::env::temp_dir().join(format!("psdb-crash-{}-{round}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let kill_after = 1 + getrandom::u32().unwrap() % 300;

        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["kill_child", "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD_DIR, &dir)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut acknowledged = 0;
        for line in BufReader::new(child.stdout.take().unwrap()).lines() {
            if line.unwrap().starts_with("acknowledged ") {
                acknowledged += 1;
                if acknowledged == kill_after {
                    break;
                }
            }
        }
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(acknowledged, kill_after, "the child stopped early");

        let ids = recovered_ids(&dir);
        assert!(
            ids.len() >= acknowledged as usize,
            "round {round}: {} recovered, {acknowledged} acknowledged",
            ids.len()
        );
        // Inserted in order 0, 1, 2, …, so an unbroken run with nothing
        // invented is exactly 0..n.
        assert!(
            ids.iter().copied().eq(0..ids.len() as i32),
            "round {round}: {ids:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

fn recovered_ids(dir: &Path) -> Vec<i32> {
    let db = Database::open(MemoryEngine::new(), dir).unwrap();
    match state(&db).orders {
        Some(docs) => docs.into_keys().collect(),
        None => Vec::new(),
    }
}
