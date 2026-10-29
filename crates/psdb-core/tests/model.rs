//! Property test: random autocommit operations agree with a plain map.

mod common;

use std::collections::BTreeMap;

use bson::{Bson, doc};
use proptest::prelude::*;

#[derive(Debug, Clone)]
enum Op {
    Insert(i32, i32),
    Replace(i32, i32),
    Delete(i32),
    Get(i32),
    FindGreater(i32),
}

fn op() -> impl Strategy<Value = Op> {
    // Few IDs and values, so duplicates, misses and matches are common.
    let id = -3..4i32;
    let v = 0..5i32;
    prop_oneof![
        (id.clone(), v.clone()).prop_map(|(i, v)| Op::Insert(i, v)),
        (id.clone(), v.clone()).prop_map(|(i, v)| Op::Replace(i, v)),
        id.clone().prop_map(Op::Delete),
        id.prop_map(Op::Get),
        v.prop_map(Op::FindGreater),
    ]
}

proptest! {
    #[test]
    fn matches_model(ops in prop::collection::vec(op(), 1..60)) {
        let db = common::shop();
        let mut model: BTreeMap<i32, i32> = BTreeMap::new();

        for op in ops {
            match op {
                Op::Insert(id, v) => {
                    let result = db.insert("shop", "orders", doc! { "_id": id, "v": v });
                    prop_assert_eq!(result.is_ok(), !model.contains_key(&id));
                    model.entry(id).or_insert(v);
                }
                Op::Replace(id, v) => {
                    let replaced = db.replace("shop", "orders", &Bson::Int32(id), doc! { "v": v }).unwrap();
                    prop_assert_eq!(replaced, model.contains_key(&id));
                    if let Some(slot) = model.get_mut(&id) {
                        *slot = v;
                    }
                }
                Op::Delete(id) => {
                    let deleted = db.delete("shop", "orders", &Bson::Int32(id)).unwrap();
                    prop_assert_eq!(deleted, model.remove(&id).is_some());
                }
                Op::Get(id) => {
                    let got = db.get("shop", "orders", &Bson::Int32(id)).unwrap();
                    prop_assert_eq!(got, model.get(&id).map(|v| doc! { "_id": id, "v": *v }));
                }
                Op::FindGreater(min) => {
                    let got: Vec<i32> = db
                        .find("shop", "orders", &doc! { "v": { "$gt": min } })
                        .unwrap()
                        .iter()
                        .map(|d| d.get_i32("_id").unwrap())
                        .collect();
                    let want: Vec<i32> = model.iter().filter(|(_, v)| **v > min).map(|(id, _)| *id).collect();
                    prop_assert_eq!(got, want);
                }
            }
        }
    }
}
