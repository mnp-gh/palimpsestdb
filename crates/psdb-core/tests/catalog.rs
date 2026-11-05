//! Namespaces and Collections in the transactional Catalog (ADR 0006).

mod common;

use common::collection;

use bson::{Bson, doc};
use psdb_core::{CollectionPath, Error, Node};
use psdb_storage_memory::MemoryEngine;

#[test]
fn create_list_drop() {
    let node = Node::new(MemoryEngine::new()).unwrap();
    node.create_namespace("b").unwrap();
    node.create_namespace("a").unwrap();
    assert_eq!(node.list_namespaces().unwrap(), ["a", "b"]);

    node.create_collection(&collection("a.y")).unwrap();
    node.create_collection(&collection("a.x")).unwrap();
    assert_eq!(node.list_collections("a").unwrap(), ["x", "y"]);

    node.drop_collection(&collection("a.x")).unwrap();
    assert_eq!(node.list_collections("a").unwrap(), ["y"]);
    node.drop_namespace("a").unwrap();
    assert_eq!(node.list_namespaces().unwrap(), ["b"]);
    assert!(matches!(
        node.list_collections("a"),
        Err(Error::NoSuchNamespace(_))
    ));
}

#[test]
fn errors_name_what_is_missing_or_taken() {
    let node = common::shop();
    assert!(matches!(
        node.create_namespace("shop"),
        Err(Error::NamespaceExists(_))
    ));
    assert!(matches!(
        node.create_collection(&collection("shop.orders")),
        Err(Error::CollectionExists { .. })
    ));
    assert!(matches!(
        node.create_collection(&collection("nope.c")),
        Err(Error::NoSuchNamespace(_))
    ));
    assert!(matches!(
        node.insert(&collection("shop.nope"), doc! {}),
        Err(Error::NoSuchCollection { .. })
    ));
    assert!(matches!(
        node.create_namespace("a.b"),
        Err(Error::InvalidName(_))
    ));
    // Collection names are checked when the Collection path is made.
    for bad in ["shop.", ".orders", "shop", "shop.orders.x"] {
        assert!(
            matches!(bad.parse::<CollectionPath>(), Err(Error::InvalidName(_))),
            "{bad:?}"
        );
    }
}

#[test]
fn ddl_rolls_back_with_its_transaction() {
    let node = common::shop();
    let mut t = node.begin();
    t.create_collection(&collection("shop.audit")).unwrap();
    t.insert(&collection("shop.audit"), doc! { "_id": 1 })
        .unwrap();
    assert_eq!(
        t.list_collections("shop").unwrap(),
        ["audit", "orders"],
        "visible to itself"
    );
    t.abort();
    assert_eq!(node.list_collections("shop").unwrap(), ["orders"]);
}

#[test]
fn recreated_collection_starts_empty() {
    let node = common::shop();
    node.insert(&collection("shop.orders"), doc! { "_id": 1 })
        .unwrap();
    node.drop_collection(&collection("shop.orders")).unwrap();
    node.create_collection(&collection("shop.orders")).unwrap();
    // A new ID, never reused, so the old Documents cannot resurface.
    assert_eq!(
        node.get(&collection("shop.orders"), &Bson::Int32(1))
            .unwrap(),
        None
    );
    assert!(
        node.find(&collection("shop.orders"), &doc! {})
            .unwrap()
            .is_empty()
    );
}

#[test]
fn recreated_namespace_starts_empty() {
    let node = common::shop();
    node.insert(&collection("shop.orders"), doc! { "_id": 1 })
        .unwrap();
    node.drop_namespace("shop").unwrap();
    node.create_namespace("shop").unwrap();
    assert!(node.list_collections("shop").unwrap().is_empty());
}

#[test]
fn catalog_is_frozen_at_the_snapshot() {
    let node = common::shop();
    let mut t1 = node.begin();
    assert_eq!(t1.list_collections("shop").unwrap(), ["orders"]);
    node.create_collection(&collection("shop.audit")).unwrap();
    assert_eq!(t1.list_collections("shop").unwrap(), ["orders"]);
    assert!(matches!(
        t1.insert(&collection("shop.audit"), doc! {}),
        Err(Error::NoSuchCollection { .. })
    ));
}

#[test]
fn write_into_concurrently_dropped_collection_aborts() {
    let node = common::shop();
    let mut t1 = node.begin();
    t1.insert(&collection("shop.orders"), doc! { "_id": 1 })
        .unwrap();
    node.drop_collection(&collection("shop.orders")).unwrap();
    let err = t1.commit().unwrap_err();
    assert!(
        matches!(err, Error::CollectionChanged) && err.is_retryable(),
        "{err}"
    );
}

#[test]
fn write_into_concurrently_dropped_namespace_aborts() {
    let node = common::shop();
    let mut t1 = node.begin();
    t1.insert(&collection("shop.orders"), doc! { "_id": 1 })
        .unwrap();
    node.drop_namespace("shop").unwrap();
    assert!(matches!(t1.commit(), Err(Error::CollectionChanged)));
}

#[test]
fn read_only_transactions_commit_even_if_a_collection_they_read_is_dropped() {
    let node = common::shop();
    node.insert(&collection("shop.orders"), doc! { "_id": 1 })
        .unwrap();
    let mut t1 = node.begin();
    node.drop_collection(&collection("shop.orders")).unwrap();
    // The Snapshot still shows the Collection and its Document.
    assert_eq!(
        t1.find(&collection("shop.orders"), &doc! {}).unwrap().len(),
        1
    );
    t1.commit()
        .expect("nothing was written, so nothing can be lost");
}

#[test]
fn concurrent_writes_to_one_collection_do_not_conflict_through_the_catalog() {
    let node = common::shop();
    let mut t1 = node.begin();
    let mut t2 = node.begin();
    t1.insert(&collection("shop.orders"), doc! { "_id": 1 })
        .unwrap();
    t2.insert(&collection("shop.orders"), doc! { "_id": 2 })
        .unwrap();
    t1.commit().unwrap();
    t2.commit().unwrap();
    assert_eq!(
        node.find(&collection("shop.orders"), &doc! {})
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn concurrent_create_of_one_name_conflicts() {
    let node = common::shop();
    let mut t1 = node.begin();
    let mut t2 = node.begin();
    t1.create_collection(&collection("shop.audit")).unwrap();
    t2.create_collection(&collection("shop.audit")).unwrap();
    t1.commit().unwrap();
    assert!(matches!(t2.commit(), Err(Error::WriteConflict)));
}

#[test]
fn concurrent_creates_of_different_names_both_commit() {
    let node = common::shop();
    let mut t1 = node.begin();
    let mut t2 = node.begin();
    t1.create_namespace("acme").unwrap();
    t2.create_namespace("globex").unwrap();
    t1.create_collection(&collection("shop.audit")).unwrap();
    t2.create_collection(&collection("shop.returns")).unwrap();
    t1.commit().unwrap();
    t2.commit().unwrap();
    assert_eq!(node.list_namespaces().unwrap(), ["acme", "globex", "shop"]);
    assert_eq!(
        node.list_collections("shop").unwrap(),
        ["audit", "orders", "returns"]
    );
}
