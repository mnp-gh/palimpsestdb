//! A durable Node: replay on open, and poisoning when the WAL fails.

mod common;

use common::{collection, durable_shop, open};

use bson::{Bson, doc};
use psdb_core::wal::SimDisk;
use psdb_core::{Error, Limits, Node};
use psdb_storage::{StorageEngine, WriteBatch};
use psdb_storage_memory::MemoryEngine;

#[test]
fn reopen_replays_every_commit() {
    let disk = SimDisk::new();
    let node = durable_shop(&disk);
    node.insert(&collection("shop.orders"), doc! { "_id": 1, "v": 1 })
        .unwrap();
    node.insert(&collection("shop.orders"), doc! { "_id": 2, "v": 2 })
        .unwrap();
    let mut txn = node.begin();
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

    let next = node.begin();
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
fn engine_with_data_is_refused() {
    let with_data = || {
        let engine = MemoryEngine::new();
        let mut batch = WriteBatch::new();
        batch.put(b"k".to_vec(), b"v".to_vec());
        engine.write_batch(batch).unwrap();
        engine
    };
    let disk = SimDisk::new();
    assert!(matches!(
        Node::open_with(with_data(), disk.file(), Limits::default()),
        Err(Error::EngineNotEmpty)
    ));
    assert!(matches!(Node::new(with_data()), Err(Error::EngineNotEmpty)));
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

    // After a power loss it is gone.
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
