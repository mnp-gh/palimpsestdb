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
use psdb_core::{Database, Limits};
use psdb_storage_memory::MemoryEngine;

/// `shop.orders` as `_id → v`, or `None` before it exists.
type State = Option<BTreeMap<i32, i32>>;

/// One write in a Transaction: set `_id` to `v`, or delete it.
type Op = (i32, Option<i32>);

fn state(db: &Database<MemoryEngine>) -> State {
    let has_orders = db.list_databases().unwrap().contains(&"shop".to_string())
        && db
            .list_collections("shop")
            .unwrap()
            .contains(&"orders".to_string());
    has_orders.then(|| {
        db.find("shop", "orders", &doc! {})
            .unwrap()
            .into_iter()
            .map(|d| (d.get_i32("_id").unwrap(), d.get_i32("v").unwrap()))
            .collect()
    })
}

/// Run `txn` as one Transaction and commit it.
fn run(db: &Database<MemoryEngine>, txn: &[Op]) -> psdb_core::Result<()> {
    let mut t = db.begin();
    for &(id, value) in txn {
        let id = Bson::Int32(id);
        match value {
            Some(v) => {
                if !t.replace("shop", "orders", &id, doc! { "v": v })? {
                    t.insert("shop", "orders", doc! { "_id": id, "v": v })?;
                }
            }
            None => {
                t.delete("shop", "orders", &id)?;
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
        // states[n] is the expected state after the first n commits.
        let mut states: Vec<State> = vec![None, None, Some(BTreeMap::new())];
        for txn in &txns {
            let mut next = states.last().unwrap().clone().unwrap();
            for &(id, value) in txn {
                match value {
                    Some(v) => next.insert(id, v),
                    None => next.remove(&id),
                };
            }
            states.push(Some(next));
        }

        let disk = SimDisk::new();
        disk.crash_after(crash_after, keep_unsynced);
        let mut acknowledged = 0;
        if let Ok(db) = Database::open_with(MemoryEngine::new(), disk.file(), Limits::default()) {
            let setup: [&dyn Fn() -> psdb_core::Result<()>; 2] = [
                &|| db.create_database("shop"),
                &|| db.create_collection("shop", "orders"),
            ];
            let commits = setup
                .iter()
                .map(|f| f())
                .chain(txns.iter().map(|t| run(&db, t)));
            for outcome in commits {
                if outcome.is_err() {
                    break;
                }
                acknowledged += 1;
            }
        }
        // If the scheduled crash never fired, cut the power now.
        disk.power_loss(keep_unsynced);

        let db = Database::open_with(MemoryEngine::new(), disk.file(), Limits::default()).unwrap();
        let recovered = state(&db);
        let in_flight = (acknowledged + 1).min(states.len() - 1);
        prop_assert!(
            recovered == states[acknowledged] || recovered == states[in_flight],
            "recovered {recovered:?} after {acknowledged} acknowledged commits"
        );

        // The recovered Database keeps working and survives another reopen.
        drop(db);
        let db = Database::open_with(MemoryEngine::new(), disk.file(), Limits::default()).unwrap();
        prop_assert_eq!(state(&db), recovered.clone());
        if recovered.is_some() {
            db.insert("shop", "orders", doc! { "_id": 99, "v": 0 }).unwrap();
            drop(db);
            let db = Database::open_with(MemoryEngine::new(), disk.file(), Limits::default()).unwrap();
            prop_assert_eq!(state(&db).unwrap().get(&99).copied(), Some(0));
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
    match state(&db) {
        Some(docs) => docs.into_keys().collect(),
        None => Vec::new(),
    }
}
