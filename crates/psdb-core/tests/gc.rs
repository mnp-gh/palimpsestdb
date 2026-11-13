//! Garbage Collection removes only Versions no open Snapshot can read.

mod common;

use common::collection;

use bson::{Bson, doc};
use psdb_core::GcStats;

#[test]
fn an_open_snapshot_keeps_the_version_it_reads() {
    let node = common::shop();
    let orders = collection("shop.orders");
    node.insert(&orders, doc! { "_id": 1, "v": "old" }).unwrap();
    let mut reader = node.begin_transaction();
    node.replace(&orders, &Bson::Int32(1), doc! { "v": "new" })
        .unwrap();

    assert_eq!(node.collect_garbage().unwrap(), GcStats::default());
    assert_eq!(
        reader.get(&orders, &Bson::Int32(1)).unwrap(),
        Some(doc! { "_id": 1, "v": "old" })
    );

    drop(reader);
    assert_eq!(node.collect_garbage().unwrap().versions_removed, 1);
    assert_eq!(
        node.get(&orders, &Bson::Int32(1)).unwrap(),
        Some(doc! { "_id": 1, "v": "new" })
    );
}

#[test]
fn a_deleted_document_leaves_nothing_behind() {
    let node = common::shop();
    let orders = collection("shop.orders");
    node.insert(&orders, doc! { "_id": 1 }).unwrap();
    node.delete(&orders, &Bson::Int32(1)).unwrap();

    assert_eq!(
        node.collect_garbage().unwrap(),
        GcStats {
            versions_removed: 2,
            tombstones_removed: 1
        }
    );
    assert_eq!(node.get(&orders, &Bson::Int32(1)).unwrap(), None);
    assert_eq!(node.collect_garbage().unwrap(), GcStats::default());
}

#[test]
fn a_dropped_collection_can_be_recreated_after_collection() {
    let node = common::shop();
    let orders = collection("shop.orders");
    node.drop_collection(&orders).unwrap();

    let stats = node.collect_garbage().unwrap();
    assert_eq!(stats.tombstones_removed, 1, "the Collection entry");
    assert!(node.get(&orders, &Bson::Int32(1)).is_err(), "still dropped");

    node.create_collection(&orders).unwrap();
    node.insert(&orders, doc! { "_id": 1 }).unwrap();
    assert_eq!(
        node.get(&orders, &Bson::Int32(1)).unwrap(),
        Some(doc! { "_id": 1 })
    );
}

#[test]
fn a_key_split_across_batches_keeps_its_newest_version() {
    let node = common::shop();
    let orders = collection("shop.orders");
    node.insert(&orders, doc! { "_id": 1, "n": 0 }).unwrap();
    for n in 1..=1500 {
        node.replace(&orders, &Bson::Int32(1), doc! { "n": n })
            .unwrap();
    }

    assert_eq!(node.collect_garbage().unwrap().versions_removed, 1500);
    assert_eq!(
        node.get(&orders, &Bson::Int32(1)).unwrap(),
        Some(doc! { "_id": 1, "n": 1500 })
    );
}
