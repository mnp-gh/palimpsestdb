//! A durable Node: replay on open, and poisoning when the WAL fails.
#![allow(
    clippy::disallowed_methods,
    reason = "durability is tested on the real filesystem"
)]

mod common;

use common::{collection, durable_shop, open};

use std::ops::ControlFlow;
use std::sync::Arc;

use bson::{Bson, doc};
use psdb_core::wal::SimDisk;
use psdb_core::{Error, FieldPath, FieldSchema, FieldType, IdGenerator, Limits, Node};
use psdb_storage::{KeyRange, StorageEngine, WriteBatch};
use psdb_storage_memory::MemoryEngine;

#[test]
fn reopen_replays_every_commit() {
    let disk = SimDisk::new();
    let node = durable_shop(&disk);
    node.insert(&collection("shop.orders"), doc! { "_id": 1, "v": 1 })
        .unwrap();
    node.insert(&collection("shop.orders"), doc! { "_id": 2, "v": 2 })
        .unwrap();
    let mut txn = node.begin_transaction();
    txn.replace(
        &collection("shop.orders"),
        &Bson::Int32(1),
        doc! { "v": 10 },
    )
    .unwrap();
    txn.delete(&collection("shop.orders"), &Bson::Int32(2))
        .unwrap();
    let last = txn.commit().unwrap();
    node.create_collection(&collection("shop.dropped")).unwrap();
    node.drop_collection(&collection("shop.dropped")).unwrap();
    drop(node);
    disk.power_loss(0);

    let node = open(&disk).unwrap();
    assert_eq!(
        node.find(&collection("shop.orders"), &doc! {}).unwrap(),
        [doc! { "_id": 1, "v": 10 }]
    );
    assert_eq!(node.list_collections("shop").unwrap(), ["orders"]);

    let next = node.begin_transaction();
    assert!(next.snapshot_ts() > last, "Snapshots see recovered commits");
    drop(next);
    node.insert(&collection("shop.orders"), doc! { "_id": 3 })
        .unwrap();
    drop(node);
    let node = open(&disk).unwrap();
    assert_eq!(
        node.find(&collection("shop.orders"), &doc! {})
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn a_data_directory_is_open_in_one_node_at_a_time() {
    let dir = std::env::temp_dir().join(format!("psdb-lock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let node = Node::open(MemoryEngine::new(), &dir).unwrap();
    assert!(matches!(
        Node::open(MemoryEngine::new(), &dir),
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock
    ));
    drop(node);
    Node::open(MemoryEngine::new(), &dir).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A durable Node over `engine` and the WAL on `disk`.
fn open_shared(
    engine: &Arc<MemoryEngine>,
    disk: &SimDisk,
) -> psdb_core::Result<Node<Arc<MemoryEngine>>> {
    Node::open_with(
        Arc::clone(engine),
        disk.file(),
        Limits::default(),
        IdGenerator::default(),
    )
}

/// Every key and value in `engine`.
fn contents(engine: &MemoryEngine) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut all = Vec::new();
    engine
        .scan(&KeyRange::all(), &mut |k, v| {
            all.push((k.to_vec(), v.to_vec()));
            ControlFlow::Continue(())
        })
        .unwrap();
    all
}

#[test]
fn reopen_over_the_same_engine_replays_only_newer_records() {
    let (engine, disk) = (Arc::new(MemoryEngine::new()), SimDisk::new());
    let node = open_shared(&engine, &disk).unwrap();
    node.create_namespace("shop").unwrap();
    node.create_collection(&collection("shop.orders")).unwrap();
    node.insert(&collection("shop.orders"), doc! { "_id": 1 })
        .unwrap();
    node.delete(&collection("shop.orders"), &Bson::Int32(1))
        .unwrap();
    let mut txn = node.begin_transaction();
    txn.insert(&collection("shop.orders"), doc! { "_id": 2 })
        .unwrap();
    let last = txn.commit().unwrap();
    // Removes `_id: 1`'s Versions; replaying its WAL Records would bring them back.
    node.collect_garbage().unwrap();
    let before = contents(&engine);
    drop(node);

    let node = open_shared(&engine, &disk).unwrap();
    assert_eq!(contents(&engine), before, "nothing replayed");
    let mut txn = node.begin_transaction();
    txn.insert(&collection("shop.orders"), doc! { "_id": 3 })
        .unwrap();
    assert_eq!(txn.commit().unwrap(), last + 1);
    drop(node);

    // The WAL agrees: a fresh engine replays every Record.
    let node = open(&disk).unwrap();
    assert_eq!(
        node.find(&collection("shop.orders"), &doc! {}).unwrap(),
        [doc! { "_id": 2 }, doc! { "_id": 3 }]
    );
}

#[test]
fn reopen_over_a_lagging_engine_replays_what_it_lacks() {
    let disk = SimDisk::new();
    let node = durable_shop(&disk);
    drop(node);
    // An engine that holds the first two commits only, as a crash may leave it.
    let engine = Arc::new(MemoryEngine::new());
    let node = open_shared(&engine, &disk).unwrap();
    drop(node);
    let node = open(&disk).unwrap();
    node.insert(&collection("shop.orders"), doc! { "_id": 1 })
        .unwrap();
    drop(node);

    let node = open_shared(&engine, &disk).unwrap();
    assert_eq!(
        node.find(&collection("shop.orders"), &doc! {}).unwrap(),
        [doc! { "_id": 1 }]
    );
}

#[test]
fn engine_ahead_of_the_wal_or_without_applied_up_to_is_refused() {
    let engine = Arc::new(MemoryEngine::new());
    durable_shop_over(&engine, &SimDisk::new());
    assert!(matches!(
        open_shared(&engine, &SimDisk::new()),
        Err(Error::Corruption(_))
    ));

    let with_data = MemoryEngine::new();
    let mut batch = WriteBatch::new();
    batch.put(b"k".to_vec(), b"v".to_vec());
    with_data.write_batch(batch).unwrap();
    let with_data = Arc::new(with_data);
    assert!(matches!(
        open_shared(&with_data, &SimDisk::new()),
        Err(Error::Corruption(_))
    ));
    assert!(matches!(Node::new(with_data), Err(Error::Corruption(_))));
}

#[test]
fn a_node_without_a_wal_reopens_its_engine() {
    let engine = Arc::new(MemoryEngine::new());
    let node = Node::new(Arc::clone(&engine)).unwrap();
    node.create_namespace("shop").unwrap();
    drop(node);
    let node = Node::new(engine).unwrap();
    assert_eq!(node.list_namespaces().unwrap(), ["shop"]);
    node.create_namespace("toys").unwrap();
}

/// Create `shop.orders` in a durable Node over `engine`, then drop it.
fn durable_shop_over(engine: &Arc<MemoryEngine>, disk: &SimDisk) {
    let node = open_shared(engine, disk).unwrap();
    node.create_namespace("shop").unwrap();
    node.create_collection(&collection("shop.orders")).unwrap();
}

#[test]
fn failed_fsync_poisons_and_leaves_the_outcome_unknown() {
    let disk = SimDisk::new();
    let node = durable_shop(&disk);
    node.insert(&collection("shop.orders"), doc! { "_id": 1 })
        .unwrap();

    disk.fail_next_sync();
    let err = node
        .insert(&collection("shop.orders"), doc! { "_id": 2 })
        .unwrap_err();
    assert!(matches!(err, Error::CommitOutcomeUnknown(_)), "{err}");
    assert!(!err.is_retryable());

    let later = node
        .insert(&collection("shop.orders"), doc! { "_id": 3 })
        .unwrap_err();
    assert!(matches!(later, Error::Poisoned), "{later}");
    assert!(
        matches!(
            node.get(&collection("shop.orders"), &Bson::Int32(1)),
            Err(Error::Poisoned)
        ),
        "even read-only commits fail"
    );
    drop(node);

    // The unsynced record is still in the page cache: reopening without a
    // power loss finds it, although its commit reported failure. This is
    // why the outcome is unknown rather than an Abort.
    let ids = |node: &Node<MemoryEngine>| -> Vec<Bson> {
        node.find(&collection("shop.orders"), &doc! {})
            .unwrap()
            .into_iter()
            .map(|d| d.get("_id").unwrap().clone())
            .collect()
    };
    assert_eq!(ids(&open(&disk).unwrap()), [Bson::Int32(1), Bson::Int32(2)]);
    // Opening synced what it replayed, so a power loss now keeps it: what
    // a Node serves is always Durable.
    disk.power_loss(0);
    assert_eq!(ids(&open(&disk).unwrap()), [Bson::Int32(1), Bson::Int32(2)]);

    // A power loss before any reopen loses it.
    let disk = SimDisk::new();
    let node = durable_shop(&disk);
    node.insert(&collection("shop.orders"), doc! { "_id": 1 })
        .unwrap();
    disk.fail_next_sync();
    node.insert(&collection("shop.orders"), doc! { "_id": 2 })
        .unwrap_err();
    drop(node);
    disk.power_loss(0);
    assert_eq!(ids(&open(&disk).unwrap()), [Bson::Int32(1)]);
}

#[test]
fn index_build_chunks_commit_through_the_poisoning_path() {
    let disk = SimDisk::new();
    let node = durable_shop(&disk);
    let orders = collection("shop.orders");
    let total: FieldPath = "total".parse().unwrap();
    node.insert(&orders, doc! { "total": 1 }).unwrap();
    let field = FieldSchema {
        field_type: FieldType::Int,
        strict: false,
    };
    node.set_field_type(&orders, &total, field).unwrap();

    disk.fail_next_sync();
    assert!(matches!(
        node.create_index(&orders, &total, false),
        Err(Error::CommitOutcomeUnknown(_))
    ));
    assert!(matches!(
        node.create_index(&orders, &total, false),
        Err(Error::Poisoned)
    ));
}

#[test]
fn failed_append_poisons() {
    let disk = SimDisk::new();
    let node = durable_shop(&disk);
    disk.fail_next_append();
    assert!(matches!(
        node.insert(&collection("shop.orders"), doc! { "_id": 1 }),
        Err(Error::CommitOutcomeUnknown(_))
    ));
    assert!(matches!(
        node.insert(&collection("shop.orders"), doc! { "_id": 2 }),
        Err(Error::Poisoned)
    ));
}
