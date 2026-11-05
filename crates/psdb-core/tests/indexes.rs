//! Indexes, the Collection Schema, Strict Fields and unique Indexes
//! (ADR 0010, ADR 0011).

mod common;

use bson::{Bson, Document, doc};
use proptest::prelude::*;
use psdb_core::wal::SimDisk;
use psdb_core::{Error, FieldSchema, FieldType, IndexInfo, Limits, Node};
use psdb_storage_memory::MemoryEngine;

fn field(field_type: FieldType, strict: bool) -> FieldSchema {
    FieldSchema { field_type, strict }
}

fn ids(docs: &[Document]) -> Vec<Bson> {
    docs.iter().map(|d| d.get("_id").unwrap().clone()).collect()
}

/// `shop.orders` with `path` declared and indexed.
fn indexed(path: &str, field_type: FieldType, strict: bool, unique: bool) -> Node<MemoryEngine> {
    let node = common::shop();
    node.set_field_type("shop", "orders", path, field(field_type, strict))
        .unwrap();
    node.create_index("shop", "orders", path, unique).unwrap();
    node
}

#[derive(Debug, Clone)]
enum Op {
    Insert(i32, Option<Bson>),
    Replace(i32, Option<Bson>),
    Delete(i32),
}

fn value() -> impl Strategy<Value = Bson> {
    // Few distinct numbers so ints, doubles and coerced keys collide often.
    prop_oneof![
        (-3i32..3).prop_map(Bson::Int32),
        (-3i64..3).prop_map(Bson::Int64),
        prop::sample::select(vec![-1.5, -0.0, 0.5, 1.0, 2.5, f64::NAN, f64::INFINITY])
            .prop_map(Bson::Double),
        prop::sample::select(vec!["a", "b", "1"]).prop_map(|s| Bson::String(s.into())),
        Just(Bson::Null),
        Just(Bson::Array(vec![Bson::Int32(1)])),
        Just(Bson::Document(doc! { "x": 1 })),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    // Negative too, so `_id` ranges cross zero.
    let id = -3..4i32;
    prop_oneof![
        (id.clone(), prop::option::of(value())).prop_map(|(i, v)| Op::Insert(i, v)),
        (id.clone(), prop::option::of(value())).prop_map(|(i, v)| Op::Replace(i, v)),
        id.prop_map(Op::Delete),
    ]
}

fn comparison() -> impl Strategy<Value = Document> {
    let op = prop::sample::select(vec!["$eq", "$gt", "$gte", "$lt", "$lte", "$ne"]);
    (op, value()).prop_map(|(op, v)| doc! { "a.v": { op: v } })
}

fn id_comparison() -> impl Strategy<Value = Document> {
    let op = prop::sample::select(vec!["$eq", "$gt", "$gte", "$lt", "$lte", "$ne"]);
    let id = prop_oneof![
        (-4i32..5).prop_map(Bson::Int32),
        (-4i64..5).prop_map(Bson::Int64),
        prop::sample::select(vec![-1.5, 0.5, 2.0, f64::NAN, f64::NEG_INFINITY])
            .prop_map(Bson::Double),
        Just(Bson::String("1".into())),
        Just(Bson::Null),
    ];
    (op, id).prop_map(|(op, v)| doc! { "_id": { op: v } })
}

fn filter() -> impl Strategy<Value = Document> {
    prop_oneof![
        comparison(),
        id_comparison(),
        (id_comparison(), comparison()).prop_map(|(x, y)| doc! { "$and": [x, y] }),
        (id_comparison(), comparison()).prop_map(|(x, y)| doc! { "$or": [x, y] }),
        (comparison(), comparison()).prop_map(|(x, y)| doc! { "$or": [x, y] }),
        (comparison(), comparison(), comparison())
            .prop_map(|(x, y, z)| doc! { "$and": [x, { "$or": [y, z] }] }),
        // `a.w` is never indexed and always missing, so this branch matches
        // every Document: an Index union over the other branch alone would
        // miss them.
        comparison().prop_map(|x| doc! { "$or": [x, { "a.w": null }] }),
    ]
}

fn doc_with(id: i32, v: &Option<Bson>) -> Document {
    match v {
        Some(v) => doc! { "_id": id, "a": { "v": v.clone() } },
        None => doc! { "_id": id, "a": {} },
    }
}

fn apply(node: &Node<MemoryEngine>, coll: &str, op: &Op) {
    // Outcomes are checked by comparing collections, not here.
    let _ = match op {
        Op::Insert(id, v) => node.insert("shop", coll, doc_with(*id, v)).map(|_| ()),
        Op::Replace(id, v) => node
            .replace("shop", coll, &Bson::Int32(*id), doc_with(*id, v))
            .map(|_| ()),
        Op::Delete(id) => node.delete("shop", coll, &Bson::Int32(*id)).map(|_| ()),
    };
}

proptest! {
    /// Whatever the writes, and whether the Index existed before them or was
    /// built after some of them, `find` through an Index returns exactly what
    /// a full scan of an unindexed Collection returns.
    #[test]
    fn index_backed_find_matches_a_full_scan(
        field_type in prop::sample::select(vec![FieldType::Int, FieldType::Double, FieldType::String]),
        ops in prop::collection::vec(op(), 0..30),
        build_after in 0..30usize,
        filters in prop::collection::vec(filter(), 1..8),
    ) {
        let node = common::shop();
        node.create_collection("shop", "plain").unwrap();
        node.set_field_type("shop", "orders", "a.v", field(field_type, false)).unwrap();
        for (n, op) in ops.iter().enumerate() {
            if n == build_after {
                node.create_index("shop", "orders", "a.v", false).unwrap();
            }
            apply(&node, "orders", op);
            apply(&node, "plain", op);
        }
        if build_after >= ops.len() {
            node.create_index("shop", "orders", "a.v", false).unwrap();
        }
        for f in filters.iter().chain([doc! { "a.v": null }].iter()) {
            // Compared by _id: a Document holding NaN never equals itself.
            let want = ids(&node.find("shop", "plain", f).unwrap());
            prop_assert_eq!(ids(&node.find("shop", "orders", f).unwrap()), want, "{}", f);
        }
    }
}

#[test]
fn numbers_compare_exactly_through_an_int_index() {
    let node = indexed("n", FieldType::Int, false, false);
    let big = 9_007_199_254_740_993i64; // 2⁵³ + 1
    node.insert("shop", "orders", doc! { "_id": 1, "n": big })
        .unwrap();
    node.insert("shop", "orders", doc! { "_id": 2, "n": 30.5 })
        .unwrap();
    node.insert("shop", "orders", doc! { "_id": 3, "n": 30 })
        .unwrap();
    let find = |f| ids(&node.find("shop", "orders", &f).unwrap());
    assert!(
        find(doc! { "n": 9_007_199_254_740_992.0 }).is_empty(),
        "2⁵³ is not 2⁵³ + 1"
    );
    assert_eq!(
        find(doc! { "n": { "$gt": 30 } }),
        [Bson::Int32(1), Bson::Int32(2)]
    );
    assert_eq!(
        find(doc! { "n": 30.5 }),
        [Bson::Int32(2)],
        "30.5 shares key 30 with 30"
    );
}

#[test]
fn membership_follows_the_documents_current_type() {
    let node = indexed("age", FieldType::Int, false, false);
    node.insert("shop", "orders", doc! { "_id": 7, "age": "30" })
        .unwrap();
    let find30 = || ids(&node.find("shop", "orders", &doc! { "age": 30 }).unwrap());
    assert!(find30().is_empty());
    node.replace("shop", "orders", &Bson::Int32(7), doc! { "age": 30 })
        .unwrap();
    assert_eq!(find30(), [Bson::Int32(7)]);
    node.replace("shop", "orders", &Bson::Int32(7), doc! { "age": "x" })
        .unwrap();
    assert!(find30().is_empty());
}

#[test]
fn snapshots_see_the_index_as_of_their_timestamp() {
    let node = indexed("age", FieldType::Int, false, false);
    node.insert("shop", "orders", doc! { "_id": 7, "age": 30 })
        .unwrap();
    let mut old = node.begin();
    node.replace("shop", "orders", &Bson::Int32(7), doc! { "age": 31 })
        .unwrap();
    assert_eq!(
        ids(&old.find("shop", "orders", &doc! { "age": 30 }).unwrap()),
        [Bson::Int32(7)]
    );
    assert!(
        old.find("shop", "orders", &doc! { "age": 31 })
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        ids(&node.find("shop", "orders", &doc! { "age": 31 }).unwrap()),
        [Bson::Int32(7)]
    );
}

#[test]
fn own_writes_are_found_through_the_index() {
    let node = indexed("age", FieldType::Int, false, false);
    node.insert("shop", "orders", doc! { "_id": 1, "age": 5 })
        .unwrap();
    let mut txn = node.begin();
    txn.insert("shop", "orders", doc! { "_id": 2, "age": 5 })
        .unwrap();
    txn.delete("shop", "orders", &Bson::Int32(1)).unwrap();
    assert_eq!(
        ids(&txn.find("shop", "orders", &doc! { "age": 5 }).unwrap()),
        [Bson::Int32(2)]
    );
}

#[test]
fn unique_index_rejects_duplicates_but_not_missing_or_null() {
    let node = indexed("email", FieldType::String, true, true);
    node.insert("shop", "orders", doc! { "_id": 1, "email": "a@x" })
        .unwrap();
    assert!(matches!(
        node.insert("shop", "orders", doc! { "_id": 2, "email": "a@x" }),
        Err(Error::DuplicateValue(_))
    ));
    for id in 3..6 {
        node.insert("shop", "orders", doc! { "_id": id }).unwrap();
        node.insert("shop", "orders", doc! { "_id": id + 10, "email": null })
            .unwrap();
    }
    // Moving a value to another Document in one Transaction is fine.
    node.run(|t| {
        t.replace("shop", "orders", &Bson::Int32(1), doc! { "email": "b@x" })?;
        t.insert("shop", "orders", doc! { "_id": 2, "email": "a@x" })
    })
    .unwrap();
    // Rewriting a Document with its own value is not a duplicate.
    node.replace(
        "shop",
        "orders",
        &Bson::Int32(2),
        doc! { "email": "a@x", "n": 1 },
    )
    .unwrap();
}

#[test]
fn unique_int_index_treats_30_and_30_0_as_the_same_value() {
    let node = indexed("n", FieldType::Int, true, true);
    node.insert("shop", "orders", doc! { "_id": 1, "n": 30 })
        .unwrap();
    assert!(matches!(
        node.insert("shop", "orders", doc! { "_id": 2, "n": 30.0 }),
        Err(Error::DuplicateValue(_))
    ));
    assert!(matches!(
        node.insert("shop", "orders", doc! { "_id": 3, "n": 30.5 }),
        Err(Error::TypeMismatch(_))
    ));
}

#[test]
fn concurrent_duplicates_conflict() {
    let node = indexed("email", FieldType::String, true, true);
    let (mut t1, mut t2) = (node.begin(), node.begin());
    t1.insert("shop", "orders", doc! { "_id": 1, "email": "a@x" })
        .unwrap();
    t2.insert("shop", "orders", doc! { "_id": 2, "email": "a@x" })
        .unwrap();
    t1.commit().unwrap();
    assert!(matches!(t2.commit(), Err(Error::WriteConflict)));
}

#[test]
fn failed_write_leaves_nothing_behind() {
    let node = common::shop();
    node.set_field_type("shop", "orders", "a", field(FieldType::Int, false))
        .unwrap();
    node.set_field_type("shop", "orders", "b", field(FieldType::Int, true))
        .unwrap();
    node.create_index("shop", "orders", "a", false).unwrap();
    node.create_index("shop", "orders", "b", true).unwrap();
    node.insert("shop", "orders", doc! { "_id": 1, "a": 1, "b": 1 })
        .unwrap();
    let mut txn = node.begin();
    let dup = txn.insert("shop", "orders", doc! { "_id": 2, "a": 2, "b": 1 });
    assert!(matches!(dup, Err(Error::DuplicateValue(_))));
    txn.commit().unwrap();
    assert!(
        node.find("shop", "orders", &doc! { "a": 2 })
            .unwrap()
            .is_empty()
    );
}

#[test]
fn strict_fields_reject_other_types() {
    let node = common::shop();
    node.set_field_type("shop", "orders", "n", field(FieldType::Int, true))
        .unwrap();
    assert!(matches!(
        node.insert("shop", "orders", doc! { "n": "thirty" }),
        Err(Error::TypeMismatch(_))
    ));
    node.insert("shop", "orders", doc! { "n": 30.0 }).unwrap();
    node.insert("shop", "orders", doc! { "n": null }).unwrap();
    node.insert("shop", "orders", doc! {}).unwrap();
    node.set_field_type("shop", "orders", "f", field(FieldType::Double, true))
        .unwrap();
    assert!(matches!(
        node.insert("shop", "orders", doc! { "f": f64::NAN }),
        Err(Error::TypeMismatch(_))
    ));
    assert!(matches!(
        node.insert("shop", "orders", doc! { "f": (1i64 << 53) + 1 }),
        Err(Error::TypeMismatch(_))
    ));
}

#[test]
fn making_a_field_strict_checks_existing_documents() {
    let node = common::shop();
    node.insert("shop", "orders", doc! { "_id": 1, "n": "thirty" })
        .unwrap();
    assert!(matches!(
        node.set_field_type("shop", "orders", "n", field(FieldType::Int, true)),
        Err(Error::TypeMismatch(_))
    ));
    assert!(
        node.collection_schema("shop", "orders").unwrap().is_empty(),
        "nothing changed"
    );
    node.set_field_type("shop", "orders", "n", field(FieldType::Int, false))
        .unwrap();
    node.delete("shop", "orders", &Bson::Int32(1)).unwrap();
    node.set_field_type("shop", "orders", "n", field(FieldType::Int, true))
        .unwrap();
}

#[test]
fn schema_rules_around_indexes() {
    let node = common::shop();
    assert!(
        matches!(
            node.create_index("shop", "orders", "n", false),
            Err(Error::InvalidSchema(_))
        ),
        "type not declared"
    );
    node.set_field_type("shop", "orders", "n", field(FieldType::Int, false))
        .unwrap();
    assert!(
        matches!(
            node.create_index("shop", "orders", "n", true),
            Err(Error::InvalidSchema(_))
        ),
        "unique needs strict"
    );
    node.create_index("shop", "orders", "n", false).unwrap();
    assert!(matches!(
        node.create_index("shop", "orders", "n", false),
        Err(Error::IndexExists { .. })
    ));
    assert!(
        matches!(
            node.set_field_type("shop", "orders", "n", field(FieldType::Double, false)),
            Err(Error::InvalidSchema(_))
        ),
        "no type change while indexed"
    );
    assert!(matches!(
        node.remove_field_type("shop", "orders", "n"),
        Err(Error::InvalidSchema(_))
    ));
    node.set_field_type("shop", "orders", "n", field(FieldType::Int, true))
        .unwrap();
    node.drop_index("shop", "orders", "n").unwrap();
    node.create_index("shop", "orders", "n", true).unwrap();
    assert!(
        matches!(
            node.set_field_type("shop", "orders", "n", field(FieldType::Int, false)),
            Err(Error::InvalidSchema(_))
        ),
        "unique index keeps the field strict"
    );
    assert_eq!(
        node.list_indexes("shop", "orders").unwrap(),
        [IndexInfo {
            path: "n".into(),
            unique: true
        }]
    );
    node.drop_index("shop", "orders", "n").unwrap();
    assert!(matches!(
        node.drop_index("shop", "orders", "n"),
        Err(Error::NoSuchIndex { .. })
    ));
    node.remove_field_type("shop", "orders", "n").unwrap();
    assert!(matches!(
        node.set_field_type("shop", "orders", "_id", field(FieldType::Int, false)),
        Err(Error::InvalidSchema(_))
    ));
}

#[test]
fn index_inside_a_transaction_needs_an_empty_collection() {
    let node = common::shop();
    node.set_field_type("shop", "orders", "n", field(FieldType::Int, false))
        .unwrap();
    node.insert("shop", "orders", doc! { "n": 1 }).unwrap();
    let mut txn = node.begin();
    assert!(matches!(
        txn.create_index("shop", "orders", "n", false),
        Err(Error::CollectionNotEmpty { .. })
    ));
    drop(txn);

    // A Collection created, typed, indexed and filled in one Transaction.
    node.run(|t| {
        t.create_collection("shop", "users")?;
        t.set_field_type("shop", "users", "age", field(FieldType::Int, false))?;
        t.create_index("shop", "users", "age", false)?;
        t.insert("shop", "users", doc! { "_id": 1, "age": 40 })
    })
    .unwrap();
    assert_eq!(
        ids(&node
            .find("shop", "users", &doc! { "age": { "$gt": 30 } })
            .unwrap()),
        [Bson::Int32(1)]
    );
}

#[test]
fn concurrent_insert_aborts_an_index_created_on_an_empty_collection() {
    let node = common::shop();
    node.set_field_type("shop", "orders", "n", field(FieldType::Int, false))
        .unwrap();
    let mut txn = node.begin();
    txn.create_index("shop", "orders", "n", false).unwrap();
    node.insert("shop", "orders", doc! { "n": 1 }).unwrap();
    assert!(matches!(txn.commit(), Err(Error::CollectionChanged)));
}

#[test]
fn concurrent_insert_aborts_making_a_field_strict() {
    let node = common::shop();
    let mut txn = node.begin();
    txn.set_field_type("shop", "orders", "n", field(FieldType::Int, true))
        .unwrap();
    node.insert("shop", "orders", doc! { "n": "thirty" })
        .unwrap();
    assert!(matches!(txn.commit(), Err(Error::CollectionChanged)));
}

#[test]
fn writer_aborts_when_indexes_change_under_it() {
    let node = common::shop();
    node.set_field_type("shop", "orders", "n", field(FieldType::Int, false))
        .unwrap();
    let mut txn = node.begin();
    txn.insert("shop", "orders", doc! { "n": 1 }).unwrap();
    node.create_index("shop", "orders", "n", false).unwrap();
    assert!(matches!(txn.commit(), Err(Error::CollectionChanged)));
}

#[test]
fn building_a_unique_index_over_duplicates_fails_and_leaves_no_index() {
    let node = common::shop();
    node.set_field_type("shop", "orders", "n", field(FieldType::Int, true))
        .unwrap();
    // Past the first chunk, so some entries are committed before the build fails.
    for i in 0..3000 {
        node.insert("shop", "orders", doc! { "_id": i, "n": i % 2500 })
            .unwrap();
    }
    assert!(matches!(
        node.create_index("shop", "orders", "n", true),
        Err(Error::DuplicateValue(_))
    ));
    assert!(node.list_indexes("shop", "orders").unwrap().is_empty());
    // The orphaned entries never surface in a later Index.
    node.create_index("shop", "orders", "n", false).unwrap();
    assert_eq!(
        node.find("shop", "orders", &doc! { "n": 7 }).unwrap().len(),
        2
    );
}

#[test]
fn built_index_survives_reopen() {
    let disk = SimDisk::new();
    {
        let node = Node::open_with(MemoryEngine::new(), disk.file(), Limits::default()).unwrap();
        node.create_namespace("shop").unwrap();
        node.create_collection("shop", "orders").unwrap();
        for i in 0..2500 {
            node.insert("shop", "orders", doc! { "_id": i, "n": i % 100 })
                .unwrap();
        }
        node.set_field_type("shop", "orders", "n", field(FieldType::Int, false))
            .unwrap();
        node.create_index("shop", "orders", "n", false).unwrap();
    }
    let engine = MemoryEngine::new();
    let node = Node::open_with(engine, disk.file(), Limits::default()).unwrap();
    let found = node.find("shop", "orders", &doc! { "n": 42 }).unwrap();
    assert_eq!(
        ids(&found),
        (0..25)
            .map(|k| Bson::Int32(k * 100 + 42))
            .collect::<Vec<_>>()
    );
}

#[test]
fn write_set_cap_fails_the_write_that_crosses_it() {
    let limits = Limits {
        max_write_set_bytes: 2000,
        ..Limits::default()
    };
    let node = Node::with_limits(MemoryEngine::new(), limits);
    node.create_namespace("d").unwrap();
    node.create_collection("d", "c").unwrap();
    let mut txn = node.begin();
    let mut written = 0;
    let err = loop {
        match txn.insert("d", "c", doc! { "_id": written, "pad": "x".repeat(100) }) {
            Ok(_) => written += 1,
            Err(e) => break e,
        }
    };
    assert!(matches!(err, Error::TransactionTooLarge), "{err}");
    assert!(!err.is_retryable());
    txn.commit().unwrap();
    assert_eq!(
        node.find("d", "c", &doc! {}).unwrap().len(),
        written as usize
    );
}

#[test]
fn largest_document_fits_the_default_write_set_cap() {
    let node = indexed("s", FieldType::String, false, false);
    let max = node.limits().max_document_bytes;
    // An indexed string almost as large as the Document: the entry key
    // carries the whole value, doubling what the write adds.
    let doc = doc! { "_id": 1, "s": "x".repeat(max - 100) };
    node.insert("shop", "orders", doc).unwrap();
    assert_eq!(
        node.find("shop", "orders", &doc! { "s": "x".repeat(max - 100) })
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn build_over_large_documents_reads_them_in_byte_bounded_chunks() {
    let node = common::shop();
    let pad = "x".repeat(1024 * 1024);
    // 40 MiB of Documents, so the build stops several chunks on bytes, not
    // on the Document count.
    for i in 0..40 {
        node.insert("shop", "orders", doc! { "_id": i, "n": i % 4, "pad": &pad })
            .unwrap();
    }
    node.set_field_type("shop", "orders", "n", field(FieldType::Int, false))
        .unwrap();
    node.create_index("shop", "orders", "n", false).unwrap();
    assert_eq!(
        node.find("shop", "orders", &doc! { "n": 3 }).unwrap().len(),
        10
    );
}

#[test]
fn refused_index_in_a_transaction_writes_nothing() {
    let node = common::shop();
    node.set_field_type("shop", "orders", "n", field(FieldType::Int, false))
        .unwrap();
    node.insert("shop", "orders", doc! { "n": 1 }).unwrap();
    let mut txn = node.begin();
    let snapshot = txn.snapshot_ts();
    assert!(matches!(
        txn.create_index("shop", "orders", "n", false),
        Err(Error::CollectionNotEmpty { .. })
    ));
    // Still read-only: committing returns the Snapshot, not a new timestamp.
    assert_eq!(txn.commit().unwrap(), snapshot);
}

#[test]
fn deleting_the_last_document_lets_a_transaction_create_an_index() {
    let node = common::shop();
    node.set_field_type("shop", "orders", "n", field(FieldType::Int, false))
        .unwrap();
    node.insert("shop", "orders", doc! { "_id": 1, "n": 1 })
        .unwrap();
    node.run(|t| {
        t.delete("shop", "orders", &Bson::Int32(1))?;
        t.create_index("shop", "orders", "n", false)
    })
    .unwrap();
}

#[test]
fn orphaned_index_entries_do_not_make_a_collection_look_non_empty() {
    let node = common::shop();
    node.set_field_type("shop", "orders", "n", field(FieldType::Int, true))
        .unwrap();
    for i in 0..3000 {
        node.insert("shop", "orders", doc! { "_id": i, "n": i % 2500 })
            .unwrap();
    }
    assert!(node.create_index("shop", "orders", "n", true).is_err());
    node.run(|t| {
        for i in 0..3000 {
            t.delete("shop", "orders", &Bson::Int32(i))?;
        }
        Ok(())
    })
    .unwrap();
    // The failed build's entries share the Collection's range, below its
    // Documents.
    assert!(node.find("shop", "orders", &doc! {}).unwrap().is_empty());
    node.run(|t| t.create_index("shop", "orders", "n", true))
        .unwrap();
}

#[test]
fn a_dropped_index_leaves_nothing_in_a_new_one() {
    let node = indexed("n", FieldType::Int, true, true);
    node.insert("shop", "orders", doc! { "_id": 1, "n": 5 })
        .unwrap();
    node.drop_index("shop", "orders", "n").unwrap();
    // Unindexed now, so the dropped Index keeps its stale entry for 5.
    node.delete("shop", "orders", &Bson::Int32(1)).unwrap();
    node.create_index("shop", "orders", "n", true).unwrap();
    node.insert("shop", "orders", doc! { "_id": 2, "n": 5 })
        .unwrap();
}

#[test]
fn making_a_field_strict_reads_only_documents_not_index_entries() {
    let node = indexed("n", FieldType::Int, false, false);
    node.insert("shop", "orders", doc! { "n": 1, "m": 2 })
        .unwrap();
    node.set_field_type("shop", "orders", "m", field(FieldType::Int, true))
        .unwrap();
}
