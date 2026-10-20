//! Insert, get, replace, delete and find on one Collection.

mod common;

use bson::{Bson, doc, oid::ObjectId, spec::BinarySubtype};
use psdb_core::{Database, Error, Limits};
use psdb_storage_memory::MemoryEngine;

#[test]
fn insert_get_replace_delete() {
    let db = common::shop();
    let id = db
        .insert("shop", "orders", doc! { "_id": 1, "item": "book" })
        .unwrap();
    assert_eq!(id, Bson::Int32(1));
    assert_eq!(
        db.get("shop", "orders", &id).unwrap(),
        Some(doc! { "_id": 1, "item": "book" })
    );

    assert!(
        db.replace("shop", "orders", &id, doc! { "item": "pen" })
            .unwrap()
    );
    assert_eq!(
        db.get("shop", "orders", &id).unwrap(),
        Some(doc! { "_id": 1, "item": "pen" })
    );

    assert!(db.delete("shop", "orders", &id).unwrap());
    assert_eq!(db.get("shop", "orders", &id).unwrap(), None);
    assert!(!db.delete("shop", "orders", &id).unwrap(), "already gone");
    assert!(
        !db.replace("shop", "orders", &id, doc! {}).unwrap(),
        "replace never creates"
    );
}

#[test]
fn generated_ids_are_uuids_in_creation_order() {
    let db = common::shop();
    let ids: Vec<Bson> = (0..50)
        .map(|n| db.insert("shop", "orders", doc! { "n": n }).unwrap())
        .collect();
    for id in &ids {
        assert!(
            matches!(id, Bson::Binary(b) if b.subtype == BinarySubtype::Uuid && b.bytes.len() == 16),
            "{id:?}"
        );
    }
    // UUIDv7 sorts by creation time, so find (which returns _id order)
    // returns Documents in insertion order.
    let found: Vec<i32> = db
        .find("shop", "orders", &doc! {})
        .unwrap()
        .iter()
        .map(|d| d.get_i32("n").unwrap())
        .collect();
    assert_eq!(found, (0..50).collect::<Vec<_>>());
}

#[test]
fn duplicate_id_is_rejected_even_across_int_widths() {
    let db = common::shop();
    db.insert("shop", "orders", doc! { "_id": 7_i32 }).unwrap();
    let err = db
        .insert("shop", "orders", doc! { "_id": 7_i64 })
        .unwrap_err();
    assert!(matches!(err, Error::DuplicateId(_)), "{err}");
}

#[test]
fn deleted_id_can_be_reused() {
    let db = common::shop();
    let id = db
        .insert("shop", "orders", doc! { "_id": "x", "v": 1 })
        .unwrap();
    db.delete("shop", "orders", &id).unwrap();
    db.insert("shop", "orders", doc! { "_id": "x", "v": 2 })
        .unwrap();
    assert_eq!(
        db.get("shop", "orders", &id).unwrap(),
        Some(doc! { "_id": "x", "v": 2 })
    );
}

#[test]
fn invalid_ids_are_rejected() {
    let db = common::shop();
    for id in [
        Bson::Double(1.5),
        Bson::Null,
        Bson::Array(vec![]),
        Bson::Document(doc! {}),
    ] {
        let err = db
            .insert("shop", "orders", doc! { "_id": id.clone() })
            .unwrap_err();
        assert!(matches!(err, Error::InvalidId(_)), "{id:?}: {err}");
    }
}

#[test]
fn limits_are_enforced() {
    let limits = Limits {
        max_document_bytes: 100,
        max_nesting_depth: 2,
    };
    let db = Database::with_limits(MemoryEngine::new(), limits);
    db.create_database("d").unwrap();
    db.create_collection("d", "c").unwrap();
    assert!(db.insert("d", "c", doc! { "a": { "b": 1 } }).is_ok());
    assert!(matches!(
        db.insert("d", "c", doc! { "a": { "b": {} } }),
        Err(Error::InvalidDocument(_))
    ));
    assert!(matches!(
        db.insert("d", "c", doc! { "s": "x".repeat(200) }),
        Err(Error::InvalidDocument(_))
    ));
}

#[test]
fn find_filters_in_id_order() {
    let db = common::shop();
    for (id, status, total) in [(3, "open", 30), (1, "paid", 10), (2, "open", 20)] {
        db.insert(
            "shop",
            "orders",
            doc! { "_id": id, "status": status, "total": total },
        )
        .unwrap();
    }
    let ids = |filter| -> Vec<Bson> {
        db.find("shop", "orders", &filter)
            .unwrap()
            .iter()
            .map(|d| d.get("_id").unwrap().clone())
            .collect()
    };
    assert_eq!(
        ids(doc! {}),
        [Bson::Int32(1), Bson::Int32(2), Bson::Int32(3)]
    );
    assert_eq!(
        ids(doc! { "status": "open" }),
        [Bson::Int32(2), Bson::Int32(3)]
    );
    assert_eq!(
        ids(doc! { "status": "open", "total": { "$gt": 25 } }),
        [Bson::Int32(3)]
    );
    assert_eq!(
        ids(doc! { "$or": [ { "_id": 1 }, { "total": 30 } ] }),
        [Bson::Int32(1), Bson::Int32(3)]
    );
}

#[test]
fn collections_are_separate() {
    let db = common::shop();
    db.create_collection("shop", "returns").unwrap();
    db.insert("shop", "orders", doc! { "_id": 1 }).unwrap();
    db.insert("shop", "returns", doc! { "_id": 1 }).unwrap();
    assert_eq!(db.find("shop", "returns", &doc! {}).unwrap().len(), 1);
    db.delete("shop", "returns", &Bson::Int32(1)).unwrap();
    assert!(db.get("shop", "orders", &Bson::Int32(1)).unwrap().is_some());
}

#[test]
fn client_supplied_object_ids_work_as_ids_and_fields() {
    // Data imported from MongoDB keeps its ObjectId _ids and references.
    let db = common::shop();
    let (order, customer) = (ObjectId::new(), ObjectId::new());
    db.insert(
        "shop",
        "orders",
        doc! { "_id": order, "customer": customer },
    )
    .unwrap();
    let err = db
        .insert("shop", "orders", doc! { "_id": order })
        .unwrap_err();
    assert!(matches!(err, Error::DuplicateId(_)), "{err}");
    assert!(
        db.get("shop", "orders", &Bson::ObjectId(order))
            .unwrap()
            .is_some()
    );
    let found = db
        .find("shop", "orders", &doc! { "customer": customer })
        .unwrap();
    assert_eq!(found.len(), 1);
}
