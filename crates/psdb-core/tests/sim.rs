//! Deterministic simulator: one thread interleaves steps of many
//! Transactions, an Index build and Garbage Collection passes over a
//! `SimDisk`, crashing at random, and checks after every step that
//! Snapshots never change, Indexes agree with their Documents, and Recovery
//! keeps every acknowledged commit.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bson::{Document, doc};
use common::{collection, field_path};
use proptest::prelude::*;
use psdb_core::wal::SimDisk;
use psdb_core::{
    Error, FieldSchema, FieldType, GcPass, IdGenerator, IndexBuild, IndexState, Limits, Node,
    Transaction,
};
use psdb_storage_memory::MemoryEngine;
use uuid::Builder;

const SLOTS: usize = 4;

#[derive(Debug, Clone)]
enum Action {
    /// Writes to an empty slot begin a Transaction there too.
    Begin(usize),
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
    attempted: usize,
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
    txn: Transaction<'node, MemoryEngine>,
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

/// Deterministic ID sources, shared across reopens so IDs never repeat.
#[derive(Clone)]
struct Seeds {
    clock: Arc<AtomicU64>,
    random: Arc<AtomicU64>,
}

impl Seeds {
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

fn open(disk: &SimDisk, seeds: &Seeds) -> Node<MemoryEngine> {
    Node::open_with(
        MemoryEngine::new(),
        disk.file(),
        Limits::default(),
        seeds.id_generator(),
    )
    .unwrap()
}

/// Run `actions` until they run out (`None`) or the process crashes:
/// `Some(docs)` holds what `orders` would hold had the commit in flight
/// succeeded, if one was.
fn run(
    node: &Node<MemoryEngine>,
    disk: &SimDisk,
    actions: &mut impl Iterator<Item = Action>,
    model: &mut Model,
) -> Result<Option<Option<BTreeMap<i32, i32>>>, TestCaseError> {
    let orders = collection("shop.orders");
    let n = field_path("n");
    let mut slots: Vec<Option<Slot<'_>>> = (0..SLOTS).map(|_| None).collect();
    let mut build: Option<IndexBuild<'_, MemoryEngine>> = None;
    let mut gc: Option<GcPass<'_, MemoryEngine>> = None;
    let begin = || Slot {
        txn: node.begin_transaction(),
        writes: BTreeMap::new(),
    };
    for action in actions {
        match action {
            Action::Begin(s) => {
                slots[s].get_or_insert_with(begin);
            }
            Action::Insert(s, id, value) => {
                let slot = slots[s].get_or_insert_with(begin);
                let present = slot.view(model).contains_key(&id);
                match slot.txn.insert(&orders, doc! { "_id": id, "n": value }) {
                    Ok(_) => {
                        prop_assert!(!present, "insert of existing {id}");
                        slot.writes.insert(id, Some(value));
                    }
                    Err(Error::DuplicateId(_)) => prop_assert!(present),
                    Err(Error::DuplicateValue(_) | Error::CollectionChanged) => {}
                    Err(e) => return Err(TestCaseError::fail(format!("insert: {e:?}"))),
                }
            }
            Action::Replace(s, id, value) => {
                let slot = slots[s].get_or_insert_with(begin);
                let present = slot.view(model).contains_key(&id);
                match slot.txn.replace(&orders, &id.into(), doc! { "n": value }) {
                    Ok(replaced) => {
                        prop_assert_eq!(replaced, present);
                        if replaced {
                            slot.writes.insert(id, Some(value));
                        }
                    }
                    Err(Error::DuplicateValue(_) | Error::CollectionChanged) => {}
                    Err(e) => return Err(TestCaseError::fail(format!("replace: {e:?}"))),
                }
            }
            Action::Delete(s, id) => {
                let slot = slots[s].get_or_insert_with(begin);
                let present = slot.view(model).contains_key(&id);
                match slot.txn.delete(&orders, &id.into()) {
                    Ok(deleted) => {
                        prop_assert_eq!(deleted, present);
                        if deleted {
                            slot.writes.insert(id, None);
                        }
                    }
                    Err(Error::CollectionChanged) => {}
                    Err(e) => return Err(TestCaseError::fail(format!("delete: {e:?}"))),
                }
            }
            Action::Commit(s) => {
                let Some(slot) = slots[s].take() else {
                    continue;
                };
                let snapshot = slot.txn.snapshot_ts();
                let mut after = model.latest().clone();
                apply(&mut after, &slot.writes);
                // A Building Index lets a duplicate of a Document it has not
                // reached yet commit; its build then fails (ADR 0013).
                let ready = node
                    .list_indexes(&orders)
                    .unwrap()
                    .iter()
                    .any(|i| i.state == IndexState::Ready);
                model.attempted += usize::from(!slot.writes.is_empty());
                match slot.txn.commit() {
                    Ok(ts) => {
                        if slot.writes.is_empty() {
                            continue;
                        }
                        prop_assert!(ts > *model.history.keys().next_back().unwrap());
                        for (&id, value) in &slot.writes {
                            let last = model.last_write.get(&id).copied().unwrap_or(0);
                            prop_assert!(last <= snapshot, "lost update of {id}");
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
                        return Ok(Some((!slot.writes.is_empty()).then_some(after)));
                    }
                    Err(
                        Error::WriteConflict | Error::CollectionChanged | Error::DuplicateValue(_),
                    ) => {}
                    Err(e) => return Err(TestCaseError::fail(format!("commit: {e:?}"))),
                }
            }
            Action::Abort(s) => slots[s] = None,
            Action::StartBuild => {
                if build.is_some() {
                    continue;
                }
                match node.start_index_build(&orders, &n, true) {
                    // One Document per chunk, so commits land between chunks.
                    Ok(b) => build = Some(b.with_chunk_documents(1)),
                    Err(e) if e.requires_reopen() => return Ok(Some(None)),
                    Err(Error::IndexExists { .. }) => {}
                    Err(e) => return Err(TestCaseError::fail(format!("start build: {e:?}"))),
                }
            }
            Action::BuildStep => {
                let Some(b) = &mut build else { continue };
                match b.step() {
                    Ok(false) => {}
                    Ok(true) | Err(Error::DuplicateValue(_) | Error::IndexBuildCancelled) => {
                        build = None;
                    }
                    Err(e) if e.requires_reopen() => return Ok(Some(None)),
                    Err(e) => return Err(TestCaseError::fail(format!("build step: {e:?}"))),
                }
            }
            Action::DropIndex => match node.drop_index(&orders, &n) {
                Ok(()) | Err(Error::NoSuchIndex { .. }) => {}
                Err(e) if e.requires_reopen() => return Ok(Some(None)),
                Err(e) => return Err(TestCaseError::fail(format!("drop index: {e:?}"))),
            },
            Action::StartGc => {
                if gc.is_none() {
                    gc = Some(node.start_garbage_collection());
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
                return Ok(Some(None));
            }
            Action::CrashLater(ops, keep) => disk.crash_after(ops, keep),
        }
        // A failed build's cleanup can poison the Node behind another error.
        let indexes = match node.list_indexes(&orders) {
            Err(Error::Poisoned) => return Ok(Some(None)),
            indexes => indexes.unwrap(),
        };
        let ready = indexes.iter().any(|i| i.state == IndexState::Ready);
        check(node, &orders, &mut slots, model, ready)?;
    }
    Ok(None)
}

/// Every open Snapshot, and a new one, reads what the model says, through
/// a full scan and through the Index when it is Ready.
fn check(
    node: &Node<MemoryEngine>,
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
    let check_slot = |slot: &mut Transaction<'_, MemoryEngine>, view: BTreeMap<i32, i32>| {
        let all = slot.find(orders, &doc! {}).unwrap();
        prop_assert_eq!(all, as_docs(&view), "at Snapshot {}", slot.snapshot_ts());
        let high = slot.find(orders, &doc! { "n": { "$gte": 2 } }).unwrap();
        prop_assert_eq!(high, as_docs(view.iter().filter(|(_, n)| **n >= 2)));
        let three = slot.find(orders, &doc! { "n": 3 }).unwrap();
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

/// Run one simulation of `actions`, with Catalog IDs drawn from `seed`.
fn simulate(actions: Vec<Action>, seed: u64) -> Result<(), TestCaseError> {
    let disk = SimDisk::new();
    let seeds = Seeds {
        clock: Arc::new(AtomicU64::new(0)),
        random: Arc::new(AtomicU64::new(seed)),
    };
    let mut node = open(&disk, &seeds);
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
        attempted: 0,
        committed: 0,
    };
    let mut actions = actions.into_iter();
    while let Some(in_flight) = run(&node, &disk, &mut actions, &mut model)? {
        drop(node);
        node = open(&disk, &seeds);
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
    // A Node that aborted everything would pass the checks above.
    if model.attempted >= 10 {
        prop_assert!(
            model.committed > 0,
            "no commit of {} succeeded",
            model.attempted
        );
    }
    Ok(())
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
