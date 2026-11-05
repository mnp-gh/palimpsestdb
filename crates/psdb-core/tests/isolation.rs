//! Snapshot Isolation with first-committer-wins, exercised by interleaving
//! Transactions on one thread.

mod common;

use bson::{Bson, doc};
use psdb_core::Error;

#[test]
fn snapshot_is_stable_while_others_commit() {
    let node = common::shop();
    node.insert("shop", "orders", doc! { "_id": 1, "v": "old" })
        .unwrap();

    let mut reader = node.begin();
    assert_eq!(
        reader.get("shop", "orders", &Bson::Int32(1)).unwrap(),
        Some(doc! { "_id": 1, "v": "old" })
    );

    node.replace("shop", "orders", &Bson::Int32(1), doc! { "v": "new" })
        .unwrap();
    node.insert("shop", "orders", doc! { "_id": 2 }).unwrap();

    assert_eq!(
        reader.get("shop", "orders", &Bson::Int32(1)).unwrap(),
        Some(doc! { "_id": 1, "v": "old" })
    );
    assert_eq!(
        reader.find("shop", "orders", &doc! {}).unwrap().len(),
        1,
        "no phantom"
    );
    reader
        .commit()
        .expect("a read-only transaction always commits");

    assert_eq!(
        node.get("shop", "orders", &Bson::Int32(1)).unwrap(),
        Some(doc! { "_id": 1, "v": "new" })
    );
}

#[test]
fn reads_see_own_writes_only() {
    let node = common::shop();
    let mut t1 = node.begin();
    t1.insert("shop", "orders", doc! { "_id": 1 }).unwrap();
    assert!(t1.get("shop", "orders", &Bson::Int32(1)).unwrap().is_some());
    assert_eq!(t1.find("shop", "orders", &doc! {}).unwrap().len(), 1);

    let mut t2 = node.begin();
    assert!(
        t2.get("shop", "orders", &Bson::Int32(1)).unwrap().is_none(),
        "uncommitted is invisible"
    );

    t1.delete("shop", "orders", &Bson::Int32(1)).unwrap();
    assert!(
        t1.find("shop", "orders", &doc! {}).unwrap().is_empty(),
        "own delete hides own insert"
    );
}

#[test]
fn aborted_writes_vanish() {
    let node = common::shop();
    let mut t = node.begin();
    t.insert("shop", "orders", doc! { "_id": 1 }).unwrap();
    drop(t);
    assert!(node.find("shop", "orders", &doc! {}).unwrap().is_empty());
}

#[test]
fn first_committer_wins() {
    let node = common::shop();
    node.insert("shop", "orders", doc! { "_id": 1, "n": 0 })
        .unwrap();
    let id = Bson::Int32(1);

    let mut t1 = node.begin();
    let mut t2 = node.begin();
    t1.replace("shop", "orders", &id, doc! { "n": 1 }).unwrap();
    t2.replace("shop", "orders", &id, doc! { "n": 2 }).unwrap();
    t1.commit().unwrap();
    let err = t2.commit().unwrap_err();
    assert!(matches!(err, Error::WriteConflict) && err.is_retryable());
    assert_eq!(
        node.get("shop", "orders", &id).unwrap(),
        Some(doc! { "_id": 1, "n": 1 })
    );
}

#[test]
fn delete_conflicts_with_concurrent_replace() {
    let node = common::shop();
    node.insert("shop", "orders", doc! { "_id": 1 }).unwrap();
    let id = Bson::Int32(1);
    let mut t1 = node.begin();
    let mut t2 = node.begin();
    t1.delete("shop", "orders", &id).unwrap();
    t2.replace("shop", "orders", &id, doc! { "n": 2 }).unwrap();
    t2.commit().unwrap();
    assert!(matches!(t1.commit(), Err(Error::WriteConflict)));
}

#[test]
fn concurrent_inserts_of_one_id_conflict() {
    let node = common::shop();
    let mut t1 = node.begin();
    let mut t2 = node.begin();
    t1.insert("shop", "orders", doc! { "_id": "x" }).unwrap();
    t2.insert("shop", "orders", doc! { "_id": "x" }).unwrap();
    t1.commit().unwrap();
    assert!(matches!(t2.commit(), Err(Error::WriteConflict)));
}

#[test]
fn transactions_span_namespaces() {
    let node = common::shop();
    node.create_namespace("archive").unwrap();
    node.create_collection("archive", "orders").unwrap();
    node.insert("shop", "orders", doc! { "_id": 1 }).unwrap();

    node.run(|t| {
        let order = t.get("shop", "orders", &Bson::Int32(1))?.unwrap();
        t.delete("shop", "orders", &Bson::Int32(1))?;
        t.insert("archive", "orders", order)
    })
    .unwrap();
    assert!(
        node.get("shop", "orders", &Bson::Int32(1))
            .unwrap()
            .is_none()
    );
    assert!(
        node.get("archive", "orders", &Bson::Int32(1))
            .unwrap()
            .is_some()
    );
}

/// Snapshot Isolation permits write skew: two Transactions each read what
/// the other writes, and both commit. Serializable (v0.5) must abort one.
#[test]
fn snapshot_isolation_permits_write_skew() {
    let node = common::shop();
    node.create_collection("shop", "doctors").unwrap();
    node.insert("shop", "doctors", doc! { "_id": "alice", "on_call": true })
        .unwrap();
    node.insert("shop", "doctors", doc! { "_id": "bob", "on_call": true })
        .unwrap();
    let on_call = doc! { "on_call": true };

    let mut t1 = node.begin();
    let mut t2 = node.begin();
    assert_eq!(t1.find("shop", "doctors", &on_call).unwrap().len(), 2);
    assert_eq!(t2.find("shop", "doctors", &on_call).unwrap().len(), 2);
    t1.replace(
        "shop",
        "doctors",
        &Bson::String("alice".into()),
        doc! { "on_call": false },
    )
    .unwrap();
    t2.replace(
        "shop",
        "doctors",
        &Bson::String("bob".into()),
        doc! { "on_call": false },
    )
    .unwrap();
    t1.commit().unwrap();
    t2.commit().unwrap();

    assert!(
        node.find("shop", "doctors", &on_call).unwrap().is_empty(),
        "nobody is on call"
    );
}
