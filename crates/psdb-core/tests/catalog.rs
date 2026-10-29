//! Databases and Collections in the transactional Catalog (ADR 0006).

mod common;

use bson::{Bson, doc};
use psdb_core::{Database, Error};
use psdb_storage_memory::MemoryEngine;

#[test]
fn create_list_drop() {
    let db = Database::new(MemoryEngine::new());
    db.create_database("b").unwrap();
    db.create_database("a").unwrap();
    assert_eq!(db.list_databases().unwrap(), ["a", "b"]);

    db.create_collection("a", "y").unwrap();
    db.create_collection("a", "x").unwrap();
    assert_eq!(db.list_collections("a").unwrap(), ["x", "y"]);

    db.drop_collection("a", "x").unwrap();
    assert_eq!(db.list_collections("a").unwrap(), ["y"]);
    db.drop_database("a").unwrap();
    assert_eq!(db.list_databases().unwrap(), ["b"]);
    assert!(matches!(
        db.list_collections("a"),
        Err(Error::NoSuchDatabase(_))
    ));
}

#[test]
fn errors_name_what_is_missing_or_taken() {
    let db = common::shop();
    assert!(matches!(
        db.create_database("shop"),
        Err(Error::DatabaseExists(_))
    ));
    assert!(matches!(
        db.create_collection("shop", "orders"),
        Err(Error::CollectionExists { .. })
    ));
    assert!(matches!(
        db.create_collection("nope", "c"),
        Err(Error::NoSuchDatabase(_))
    ));
    assert!(matches!(
        db.insert("shop", "nope", doc! {}),
        Err(Error::NoSuchCollection { .. })
    ));
    assert!(matches!(
        db.create_database("a.b"),
        Err(Error::InvalidName(_))
    ));
    assert!(matches!(
        db.create_collection("shop", ""),
        Err(Error::InvalidName(_))
    ));
}

#[test]
fn ddl_rolls_back_with_its_transaction() {
    let db = common::shop();
    let mut t = db.begin();
    t.create_collection("shop", "audit").unwrap();
    t.insert("shop", "audit", doc! { "_id": 1 }).unwrap();
    assert_eq!(
        t.list_collections("shop").unwrap(),
        ["audit", "orders"],
        "visible to itself"
    );
    t.abort();
    assert_eq!(db.list_collections("shop").unwrap(), ["orders"]);
}

#[test]
fn recreated_collection_starts_empty() {
    let db = common::shop();
    db.insert("shop", "orders", doc! { "_id": 1 }).unwrap();
    db.drop_collection("shop", "orders").unwrap();
    db.create_collection("shop", "orders").unwrap();
    // A new ID, never reused, so the old Documents cannot resurface.
    assert_eq!(db.get("shop", "orders", &Bson::Int32(1)).unwrap(), None);
    assert!(db.find("shop", "orders", &doc! {}).unwrap().is_empty());
}

#[test]
fn recreated_database_starts_empty() {
    let db = common::shop();
    db.insert("shop", "orders", doc! { "_id": 1 }).unwrap();
    db.drop_database("shop").unwrap();
    db.create_database("shop").unwrap();
    assert!(db.list_collections("shop").unwrap().is_empty());
}

#[test]
fn catalog_is_frozen_at_the_snapshot() {
    let db = common::shop();
    let mut t1 = db.begin();
    assert_eq!(t1.list_collections("shop").unwrap(), ["orders"]);
    db.create_collection("shop", "audit").unwrap();
    assert_eq!(t1.list_collections("shop").unwrap(), ["orders"]);
    assert!(matches!(
        t1.insert("shop", "audit", doc! {}),
        Err(Error::NoSuchCollection { .. })
    ));
}

#[test]
fn write_into_concurrently_dropped_collection_aborts() {
    let db = common::shop();
    let mut t1 = db.begin();
    t1.insert("shop", "orders", doc! { "_id": 1 }).unwrap();
    db.drop_collection("shop", "orders").unwrap();
    let err = t1.commit().unwrap_err();
    assert!(
        matches!(err, Error::CollectionChanged) && err.is_retryable(),
        "{err}"
    );
}

#[test]
fn write_into_concurrently_dropped_database_aborts() {
    let db = common::shop();
    let mut t1 = db.begin();
    t1.insert("shop", "orders", doc! { "_id": 1 }).unwrap();
    db.drop_database("shop").unwrap();
    assert!(matches!(t1.commit(), Err(Error::CollectionChanged)));
}

#[test]
fn read_only_transactions_commit_even_if_a_collection_they_read_is_dropped() {
    let db = common::shop();
    db.insert("shop", "orders", doc! { "_id": 1 }).unwrap();
    let mut t1 = db.begin();
    db.drop_collection("shop", "orders").unwrap();
    // The Snapshot still shows the Collection and its Document.
    assert_eq!(t1.find("shop", "orders", &doc! {}).unwrap().len(), 1);
    t1.commit()
        .expect("nothing was written, so nothing can be lost");
}

#[test]
fn concurrent_writes_to_one_collection_do_not_conflict_through_the_catalog() {
    let db = common::shop();
    let mut t1 = db.begin();
    let mut t2 = db.begin();
    t1.insert("shop", "orders", doc! { "_id": 1 }).unwrap();
    t2.insert("shop", "orders", doc! { "_id": 2 }).unwrap();
    t1.commit().unwrap();
    t2.commit().unwrap();
    assert_eq!(db.find("shop", "orders", &doc! {}).unwrap().len(), 2);
}

#[test]
fn concurrent_create_of_one_name_conflicts() {
    let db = common::shop();
    let mut t1 = db.begin();
    let mut t2 = db.begin();
    t1.create_collection("shop", "audit").unwrap();
    t2.create_collection("shop", "audit").unwrap();
    t1.commit().unwrap();
    assert!(matches!(t2.commit(), Err(Error::WriteConflict)));
}

#[test]
fn concurrent_creates_of_different_names_both_commit() {
    let db = common::shop();
    let mut t1 = db.begin();
    let mut t2 = db.begin();
    t1.create_database("acme").unwrap();
    t2.create_database("globex").unwrap();
    t1.create_collection("shop", "audit").unwrap();
    t2.create_collection("shop", "returns").unwrap();
    t1.commit().unwrap();
    t2.commit().unwrap();
    assert_eq!(db.list_databases().unwrap(), ["acme", "globex", "shop"]);
    assert_eq!(
        db.list_collections("shop").unwrap(),
        ["audit", "orders", "returns"]
    );
}
