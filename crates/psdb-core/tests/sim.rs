//! Deterministic simulator: one thread interleaves steps of many
//! Transactions, an Index build and Garbage Collection passes over a
//! `SimDisk`, crashing at random, and checks after every step that
//! Snapshots never change, Indexes agree with their Documents, and Recovery
//! keeps every acknowledged commit. A crash also keeps only a random prefix
//! of the engine's batches (ADR 0015).
#![expect(clippy::unwrap_used, reason = "test code: a failure should panic")]

mod common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bson::{Document, doc};
use common::{collection, field_path};
use proptest::prelude::*;
use psdb_core::wal::SimDisk;
use psdb_core::{
    Error, FieldSchema, FieldType, GcPass, IdGenerator, IndexBuild, IndexState, Limits, Node,
    Transaction,
};
use psdb_storage::{KeyRange, ScanVisitor, StorageEngine, WriteBatch};
use psdb_storage_memory::MemoryEngine;
use uuid::Builder;

const SLOTS: usize = 4;

#[derive(Debug, Clone)]
enum Action {
    /// Writes to an empty slot begin a Transaction there too.
    Begin(usize),
    Get(usize, i32),
    Insert(usize, i32, i32),
    Replace(usize, i32, i32),
    Delete(usize, i32),
    Commit(usize),
    Abort(usize),
    StartBuild,
    BuildStep,
    DropIndex,
    StartGc,
    GcStep,
    /// Cut the power between steps, keeping this many unsynced bytes.
    Crash(usize),
    /// Cut the power during the WAL operation after this many more succeed.
    CrashLater(usize, usize),
}

fn action() -> impl Strategy<Value = Action> {
    let slot = 0..SLOTS;
    // Few IDs and values, so conflicts and duplicate values are common.
    let id = 0..6i32;
    let n = 0..5i32;
    let keep = prop_oneof![Just(0), Just(usize::MAX), 0..200usize];
    prop_oneof![
        6 => slot.clone().prop_map(Action::Begin),
        6 => (slot.clone(), id.clone()).prop_map(|(s, i)| Action::Get(s, i)),
        12 => (slot.clone(), id.clone(), n.clone()).prop_map(|(s, i, n)| Action::Insert(s, i, n)),
        9 => (slot.clone(), id.clone(), n).prop_map(|(s, i, n)| Action::Replace(s, i, n)),
        6 => (slot.clone(), id).prop_map(|(s, i)| Action::Delete(s, i)),
        12 => slot.clone().prop_map(Action::Commit),
        2 => slot.prop_map(Action::Abort),
        2 => Just(Action::StartBuild),
        6 => Just(Action::BuildStep),
        1 => Just(Action::DropIndex),
        2 => Just(Action::StartGc),
        6 => Just(Action::GcStep),
        1 => keep.clone().prop_map(Action::Crash),
        1 => (0..6usize, keep).prop_map(|(ops, k)| Action::CrashLater(ops, k)),
    ]
}

/// Committed `orders` contents (`_id` → `n`) at each Commit Timestamp that
/// changed them.
struct Model {
    history: BTreeMap<u64, BTreeMap<i32, i32>>,
    /// Commit Timestamp of each `_id`'s last write.
    last_write: BTreeMap<i32, u64>,
    committed: usize,
}

impl Model {
    fn at(&self, snapshot: u64) -> &BTreeMap<i32, i32> {
        self.history.range(..=snapshot).next_back().unwrap().1
    }

    fn latest(&self) -> &BTreeMap<i32, i32> {
        self.history.values().next_back().unwrap()
    }

    /// Forget history after Recovery: no Snapshot from before it survives.
    fn reset(&mut self, ts: u64, docs: BTreeMap<i32, i32>) {
        self.history = BTreeMap::from([(ts, docs)]);
        self.last_write.clear();
    }
}

struct Slot<'node> {
    txn: Transaction<'node, Engine>,
    /// `_id` → new `n`, or `None` for a delete.
    writes: BTreeMap<i32, Option<i32>>,
}

impl Slot<'_> {
    /// What this Transaction should read: its Snapshot plus its own writes.
    fn view(&self, model: &Model) -> BTreeMap<i32, i32> {
        let mut docs = model.at(self.txn.snapshot_ts()).clone();
        apply(&mut docs, &self.writes);
        docs
    }

    /// Run a get, insert, replace or delete and check it against the
    /// model. Returns why to stop, if the Node crashed.
    fn operate(
        &mut self,
        orders: &psdb_core::CollectionPath,
        model: &Model,
        action: &Action,
    ) -> Result<Option<Stop>, TestCaseError> {
        let view = self.view(model);
        match *action {
            Action::Get(_, id) => {
                let expected = view.get(&id).map(|&n| doc! { "_id": id, "n": n });
                match self.txn.get(orders, &id.into()) {
                    Ok(found) => prop_assert_eq!(found, expected, "get {}", id),
                    Err(e) => return crash_or_fail("get", &e).map(Some),
                }
            }
            Action::Insert(_, id, value) => {
                let present = view.contains_key(&id);
                match self.txn.insert(orders, doc! { "_id": id, "n": value }) {
                    Ok(_) => {
                        prop_assert!(!present, "insert of existing {id}");
                        self.writes.insert(id, Some(value));
                    }
                    Err(Error::DuplicateId(_)) => prop_assert!(present),
                    Err(Error::DuplicateValue(_)) => {
                        prop_assert!(held_by_another(id, value, &[&view, model.latest()]));
                    }
                    Err(Error::CollectionChanged) => {}
                    Err(e) => return crash_or_fail("insert", &e).map(Some),
                }
            }
            Action::Replace(_, id, value) => {
                let present = view.contains_key(&id);
                match self.txn.replace(orders, &id.into(), doc! { "n": value }) {
                    Ok(replaced) => {
                        prop_assert_eq!(replaced, present);
                        if replaced {
                            self.writes.insert(id, Some(value));
                        }
                    }
                    Err(Error::DuplicateValue(_)) => {
                        prop_assert!(held_by_another(id, value, &[&view, model.latest()]));
                    }
                    Err(Error::CollectionChanged) => {}
                    Err(e) => return crash_or_fail("replace", &e).map(Some),
                }
            }
            Action::Delete(_, id) => {
                let present = view.contains_key(&id);
                match self.txn.delete(orders, &id.into()) {
                    Ok(deleted) => {
                        prop_assert_eq!(deleted, present);
                        if deleted {
                            self.writes.insert(id, None);
                        }
                    }
                    Err(Error::CollectionChanged) => {}
                    Err(e) => return crash_or_fail("delete", &e).map(Some),
                }
            }
            _ => unreachable!("not a Transaction operation: {action:?}"),
        }
        Ok(None)
    }

    /// Commit and check the result against the model, recording it there
    /// on success. `ready`: whether `orders` has a Ready unique Index.
    /// Returns why to stop, if the Node crashed.
    fn commit(self, model: &mut Model, ready: bool) -> Result<Option<Stop>, TestCaseError> {
        let snapshot = self.txn.snapshot_ts();
        let mut after = model.latest().clone();
        apply(&mut after, &self.writes);
        match self.txn.commit() {
            Ok(_) if self.writes.is_empty() => {}
            Ok(ts) => {
                prop_assert!(ts > *model.history.keys().next_back().unwrap());
                for (&id, value) in &self.writes {
                    let last = model.last_write.get(&id).copied().unwrap_or(0);
                    prop_assert!(last <= snapshot, "lost update of {id}");
                    // A Building Index lets a duplicate of a Document it
                    // has not reached yet commit; its build then fails
                    // (ADR 0013).
                    if let (true, Some(value)) = (ready, value) {
                        let sharing = after.values().filter(|&v| v == value).count();
                        prop_assert_eq!(sharing, 1, "duplicate n {} committed", value);
                    }
                    model.last_write.insert(id, ts);
                }
                model.history.insert(ts, after);
                model.committed += 1;
            }
            Err(Error::CommitOutcomeUnknown(_)) => {
                return Ok(Some(Stop::Crashed {
                    in_flight: (!self.writes.is_empty()).then_some(after),
                    ready_survives: false,
                }));
            }
            Err(Error::DuplicateValue(_)) => {
                // The commit would have left two Documents sharing a value.
                let mut values: Vec<_> = after.values().collect();
                let len = values.len();
                values.sort();
                values.dedup();
                prop_assert!(values.len() < len, "DuplicateValue with no duplicate");
            }
            Err(Error::WriteConflict | Error::CollectionChanged) => {}
            Err(e) => return crash_or_fail("commit", &e).map(Some),
        }
        Ok(None)
    }
}

/// Whether a Document other than `id` holds `n` in any of `states`: the
/// only reason a unique Index may refuse a write of `n` to `id`.
fn held_by_another(id: i32, n: i32, states: &[&BTreeMap<i32, i32>]) -> bool {
    states
        .iter()
        .any(|docs| docs.iter().any(|(&other, &v)| other != id && v == n))
}

fn apply(docs: &mut BTreeMap<i32, i32>, writes: &BTreeMap<i32, Option<i32>>) {
    for (&id, &n) in writes {
        match n {
            Some(n) => docs.insert(id, n),
            None => docs.remove(&id),
        };
    }
}

fn as_docs<'a>(docs: impl IntoIterator<Item = (&'a i32, &'a i32)>) -> Vec<Document> {
    docs.into_iter()
        .map(|(&id, &n)| doc! { "_id": id, "n": n })
        .collect()
}

/// Counters behind deterministic IDs, shared across reopens so IDs never
/// repeat.
struct IdCounters {
    clock: Arc<AtomicU64>,
    random: Arc<AtomicU64>,
}

impl IdCounters {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "splitmix64 keeps the low 32 bits"
    )]
    fn id_generator(&self) -> IdGenerator {
        let clock = self.clock.clone();
        let random = self.random.clone();
        IdGenerator {
            // One millisecond per ID, so they sort in creation order.
            document_id: Box::new(move || {
                let ms = clock.fetch_add(1, Ordering::Relaxed);
                Builder::from_unix_timestamp_millis(ms, &[0; 10]).into_uuid()
            }),
            // splitmix64
            catalog_id: Box::new(move || {
                let mut z = random.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);
                z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
                (z ^ (z >> 31)) as u32
            }),
        }
    }
}

/// A `MemoryEngine` that logs every batch, so a crash can keep any prefix
/// of them.
#[derive(Default)]
struct LoggedEngine {
    engine: MemoryEngine,
    log: Mutex<Vec<WriteBatch>>,
}

/// Shared, so the engine outlives the Node a crash drops.
type Engine = Arc<LoggedEngine>;

impl LoggedEngine {
    /// The engine a crash leaves: the first `keep` batches written to this one.
    fn crashed(&self, keep: usize) -> Engine {
        let engine = Engine::default();
        for batch in self.log.lock().unwrap().iter().take(keep) {
            engine.write_batch(batch.clone()).unwrap();
        }
        engine
    }
}

impl StorageEngine for LoggedEngine {
    fn get(&self, key: &[u8]) -> psdb_storage::Result<Option<Vec<u8>>> {
        self.engine.get(key)
    }

    fn scan(&self, range: &KeyRange, visit: &mut ScanVisitor<'_>) -> psdb_storage::Result<()> {
        self.engine.scan(range, visit)
    }

    fn write_batch(&self, batch: WriteBatch) -> psdb_storage::Result<()> {
        // Held across the write, so the log keeps the engine's batch order.
        let mut log = self.log.lock().unwrap();
        self.engine.write_batch(batch.clone())?;
        log.push(batch);
        Ok(())
    }

    fn flush(&self) -> psdb_storage::Result<()> {
        Ok(())
    }
}

fn open(engine: &Engine, disk: &SimDisk, counters: &IdCounters) -> Node<Engine> {
    Node::open_with(
        Arc::clone(engine),
        disk.file(),
        Limits::default(),
        counters.id_generator(),
    )
    .unwrap()
}

/// Why [`run`] stopped.
enum Stop {
    /// The actions ran out.
    Done,
    /// The process crashed and the Node must be reopened.
    Crashed {
        /// What `orders` would hold had the commit in flight succeeded, if
        /// one was.
        in_flight: Option<BTreeMap<i32, i32>>,
        /// Whether a Ready Index must survive: the crash came between
        /// steps, so no Index commit was in flight.
        ready_survives: bool,
    },
}

/// A crash with nothing in flight if `err` needs a reopen; otherwise a
/// test failure.
fn crash_or_fail(what: &str, err: &Error) -> Result<Stop, TestCaseError> {
    if err.requires_reopen() {
        return Ok(Stop::Crashed {
            in_flight: None,
            ready_survives: false,
        });
    }
    Err(TestCaseError::fail(format!("{what}: {err:?}")))
}

/// Whether `orders` has a Ready Index; a crash if the Node is poisoned.
fn ready_index(node: &Node<Engine>) -> Result<Result<bool, Stop>, TestCaseError> {
    match node.list_indexes(&collection("shop.orders")) {
        Ok(indexes) => Ok(Ok(indexes.iter().any(|i| i.state == IndexState::Ready))),
        Err(e) => crash_or_fail("list indexes", &e).map(Err),
    }
}

/// Run `actions` until they run out or the process crashes.
fn run(
    node: &Node<Engine>,
    disk: &SimDisk,
    actions: &mut impl Iterator<Item = Action>,
    model: &mut Model,
) -> Result<Stop, TestCaseError> {
    let orders = collection("shop.orders");
    let n_path = field_path("n");
    let mut ready = match ready_index(node)? {
        Ok(ready) => ready,
        Err(stop) => return Ok(stop),
    };
    let mut slots: Vec<Option<Slot<'_>>> = (0..SLOTS).map(|_| None).collect();
    let mut build: Option<IndexBuild<'_, Engine>> = None;
    let mut gc: Option<GcPass<'_, Engine>> = None;
    let begin = || Slot {
        txn: node.begin_transaction(),
        writes: BTreeMap::new(),
    };
    for action in actions {
        match action {
            Action::Begin(s) => {
                slots[s].get_or_insert_with(begin);
            }
            Action::Get(s, _)
            | Action::Insert(s, ..)
            | Action::Replace(s, ..)
            | Action::Delete(s, _) => {
                let slot = slots[s].get_or_insert_with(begin);
                if let Some(stop) = slot.operate(&orders, model, &action)? {
                    return Ok(stop);
                }
            }
            Action::Commit(s) => {
                let Some(slot) = slots[s].take() else {
                    continue;
                };
                if let Some(stop) = slot.commit(model, ready)? {
                    return Ok(stop);
                }
            }
            Action::Abort(s) => slots[s] = None,
            Action::StartBuild => {
                if build.is_some() {
                    continue;
                }
                match node.start_index_build(&orders, &n_path, true) {
                    // One Document per chunk, so commits land between chunks.
                    Ok(b) => build = Some(b.with_chunk_documents(1)),
                    Err(Error::IndexExists { .. }) => {}
                    Err(e) => return crash_or_fail("start build", &e),
                }
            }
            Action::BuildStep => {
                let Some(b) = &mut build else { continue };
                match b.step() {
                    Ok(false) => {}
                    Ok(true) | Err(Error::DuplicateValue(_) | Error::IndexBuildCancelled) => {
                        build = None;
                    }
                    Err(e) => return crash_or_fail("build step", &e),
                }
            }
            Action::DropIndex => match node.drop_index(&orders, &n_path) {
                Ok(()) | Err(Error::NoSuchIndex { .. }) => {}
                Err(e) => return crash_or_fail("drop index", &e),
            },
            Action::StartGc => {
                if gc.is_none() {
                    // Small batches, so commits and crashes land between them;
                    // smaller ones rarely get past the Catalog before a crash.
                    gc = Some(node.start_garbage_collection().with_batch_keys(8));
                }
            }
            Action::GcStep => {
                if let Some(pass) = &mut gc
                    && pass.step().unwrap().is_some()
                {
                    gc = None;
                }
            }
            Action::Crash(keep) => {
                disk.power_loss(keep);
                return Ok(Stop::Crashed {
                    in_flight: None,
                    ready_survives: ready,
                });
            }
            Action::CrashLater(ops, keep) => disk.crash_after(ops, keep),
        }
        // A failed build's cleanup can poison the Node behind another error.
        ready = match ready_index(node)? {
            Ok(ready) => ready,
            Err(stop) => return Ok(stop),
        };
        check(node, &orders, &mut slots, model, ready)?;
    }
    Ok(Stop::Done)
}

/// Every open Snapshot, and a new one, reads what the model says, through
/// a full scan and through the Index when it is Ready.
fn check(
    node: &Node<Engine>,
    orders: &psdb_core::CollectionPath,
    slots: &mut [Option<Slot<'_>>],
    model: &Model,
    ready: bool,
) -> Result<(), TestCaseError> {
    if ready {
        let mut values: Vec<_> = model.latest().values().collect();
        let len = values.len();
        values.sort();
        values.dedup();
        prop_assert_eq!(values.len(), len, "a Ready unique Index over duplicates");
    }
    let check_slot = |txn: &mut Transaction<'_, Engine>, view: BTreeMap<i32, i32>| {
        let all = txn
            .find(orders, &doc! {})
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        prop_assert_eq!(all, as_docs(&view), "at Snapshot {}", txn.snapshot_ts());
        let high = txn
            .find(orders, &doc! { "n": { "$gte": 2 } })
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        prop_assert_eq!(high, as_docs(view.iter().filter(|(_, n)| **n >= 2)));
        let three = txn
            .find(orders, &doc! { "n": 3 })
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        prop_assert_eq!(three, as_docs(view.iter().filter(|(_, n)| **n == 3)));
        Ok(())
    };
    check_slot(&mut node.begin_transaction(), model.latest().clone())?;
    for slot in slots.iter_mut().flatten() {
        let view = slot.view(model);
        check_slot(&mut slot.txn, view)?;
    }
    Ok(())
}

/// Run one simulation of `actions`, with Catalog IDs drawn from `seed`, and
/// return how many writing commits succeeded.
fn simulate(actions: Vec<Action>, seed: u64) -> Result<usize, TestCaseError> {
    let disk = SimDisk::new();
    let counters = IdCounters {
        clock: Arc::new(AtomicU64::new(0)),
        random: Arc::new(AtomicU64::new(seed)),
    };
    let mut engine = Engine::default();
    // Picks how many engine batches each crash keeps.
    let mut lag = seed;
    let mut node = open(&engine, &disk, &counters);
    let orders = collection("shop.orders");
    let junk = collection("shop.junk");
    node.create_namespace("shop").unwrap();
    node.create_collection(&orders).unwrap();
    let strict_int = FieldSchema {
        field_type: FieldType::Int,
        strict: true,
    };
    node.set_field_type(&orders, &field_path("n"), strict_int)
        .unwrap();
    // A dropped Collection, for Garbage Collection to reclaim.
    node.create_collection(&junk).unwrap();
    node.insert(&junk, doc! { "_id": 1 }).unwrap();
    node.drop_collection(&junk).unwrap();

    let start = node.begin_transaction().snapshot_ts();
    let mut model = Model {
        history: BTreeMap::from([(start, BTreeMap::new())]),
        last_write: BTreeMap::new(),
        committed: 0,
    };
    let mut actions = actions.into_iter();
    while let Stop::Crashed {
        in_flight,
        ready_survives,
    } = run(&node, &disk, &mut actions, &mut model)?
    {
        drop(node);
        lag = lag
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let written = engine.log.lock().unwrap().len();
        engine = engine.crashed((lag >> 33) as usize % (written + 1));
        node = open(&engine, &disk, &counters);
        let indexes = node.list_indexes(&orders).unwrap();
        prop_assert!(
            indexes.iter().all(|i| i.state == IndexState::Ready),
            "Recovery left a build: {:?}",
            indexes
        );
        prop_assert!(
            !ready_survives || !indexes.is_empty(),
            "a Ready Index was lost"
        );
        let found = node.find(&orders, &doc! {}).unwrap();
        let acknowledged = as_docs(model.latest());
        let recovered = match in_flight {
            Some(docs) if found == as_docs(&docs) && found != acknowledged => docs,
            _ => {
                prop_assert_eq!(&found, &acknowledged, "an acknowledged commit was lost");
                model.latest().clone()
            }
        };
        let ts = node.begin_transaction().snapshot_ts();
        model.reset(ts, recovered);
    }
    Ok(model.committed)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn simulated_interleavings_keep_every_invariant(
        actions in prop::collection::vec(action(), 1..200),
        seed: u64,
    ) {
        simulate(actions, seed)?;
    }
}

/// A Node that aborted everything would pass every check above.
#[test]
fn independent_commits_succeed() {
    use Action::*;
    let actions = vec![Insert(0, 0, 0), Commit(0), Insert(1, 1, 1), Commit(1)];
    assert_eq!(simulate(actions, 0).unwrap(), 2);
}

/// Deleting a not-yet-indexed duplicate during a unique build must not
/// remove the entry of the Document the build already indexed.
#[test]
fn a_delete_during_a_build_keeps_another_documents_unique_entry() {
    use Action::*;
    let actions = vec![
        Insert(0, 0, 1),
        Insert(0, 2, 1),
        Commit(0),
        StartBuild,
        BuildStep,
        Delete(1, 2),
        Commit(1),
        BuildStep,
        Insert(2, 1, 1),
        Commit(2),
    ];
    simulate(actions, 0).unwrap();
}

/// A delete whose Snapshot predates the chunk that indexed its Document
/// must conflict, not commit and leave the entry behind to block the value.
#[test]
fn a_delete_racing_the_chunk_that_indexed_it_leaves_no_stale_entry() {
    use Action::*;
    let actions = vec![
        Insert(0, 0, 1),
        Insert(0, 9, 3),
        Commit(0),
        StartBuild,
        Begin(1),
        BuildStep,
        Delete(1, 0),
        Commit(1),
        BuildStep,
        Delete(2, 0),
        Commit(2),
        Insert(3, 5, 1),
        Commit(3),
    ];
    // Commits 0, 2 and 3; commit 1 conflicts with the chunk.
    assert_eq!(simulate(actions, 0).unwrap(), 3);
}
