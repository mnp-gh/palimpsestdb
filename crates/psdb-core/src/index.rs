//! Indexes: creating, building and dropping them, their entry keys, and
//! choosing an Index or a range of `_id`s for a filter (ADR 0010, ADR 0011).
//!
//! An Index has no type of its own: it covers values of its field's Field
//! Type, read from the Collection Schema (see [`crate::schema`]). Values of
//! other types are left out of it, except that an Index on an `int` field
//! files a double under its floor and one on a `double` field files an
//! integer as the nearest double, because filters compare integers and
//! doubles as numbers.
//!
//! An Index entry's logical key is `namespace_id ‖ collection_id ‖ 0x00 ‖
//! index_id ‖ component ‖ document_id`, inside its Collection's range but
//! below every Document, where the component encodes the Document's value in the
//! indexed field (see [`crate::keys`]). A unique Index leaves the Document
//! ID out, so two Transactions writing the same value write the same key
//! and the second to commit gets a Write Conflict; null and missing keep
//! it, because they never collide. The value of every entry is the encoded
//! Document ID, so a lookup can fetch the Document.
//!
//! Entries point to Document IDs, never to where a Version is stored, so a
//! write that leaves the indexed value unchanged writes nothing to the
//! Index. A lookup always fetches the Document at its Snapshot and rechecks
//! the whole filter, which also makes coerced keys safe: an Index on an
//! `int` field files `30.5` under `30`, and range scans include their
//! bounds.

use std::ops::ControlFlow;

use bson::{Bson, RawDocument};
use psdb_storage::{KeyRange, StorageEngine};

use crate::catalog::CollectionEntry;
use crate::collection_path::CollectionPath;
use crate::document::{self, lookup_raw};
use crate::error::{Error, Result};
use crate::field_path::FieldPath;
use crate::filter::{CompareOp, Filter};
use crate::keys;
use crate::mvcc;
use crate::node::Node;
use crate::schema::FieldType;
use crate::transaction::{CommitState, Transaction, WriteSet};

/// An Index build commits the entries for at most this many Documents at a
/// time...
const BUILD_CHUNK_DOCUMENTS: usize = 1024;
/// ...and of at most this many bytes of Documents, so neither its memory
/// nor its WAL Record grows with Document size. A chunk always holds at
/// least one Document, so one larger than this, allowed by raising
/// [`Limits::max_document_bytes`](crate::Limits::max_document_bytes), is a
/// chunk of its own.
const BUILD_CHUNK_BYTES: usize = 16 * 1024 * 1024;

/// An Index as clients see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexInfo {
    /// The indexed field's path, such as `address.city`.
    pub path: FieldPath,
    /// Whether no two Documents may share a value in the field.
    pub unique: bool,
}

/// An Index as the Catalog stores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexDef {
    pub(crate) path: FieldPath,
    /// Unique within its Collection and never reused there (ADR 0006).
    pub(crate) id: u32,
    pub(crate) unique: bool,
}

/// A Document's entry in an Index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexEntry {
    /// The entry's logical key.
    pub(crate) key: Vec<u8>,
    /// Whether no other Document may have this key: true in a unique Index,
    /// except for null and missing, which never collide.
    pub(crate) unique: bool,
}

impl IndexEntry {
    /// The entry in `index`, whose keys start with `prefix`, for the
    /// Document with encoded ID `doc_id` and key component `component`.
    pub(crate) fn new(prefix: Vec<u8>, index: &IndexDef, component: &[u8], doc_id: &[u8]) -> Self {
        let unique = index.unique && !keys::is_null(component);
        let mut key = prefix;
        key.extend_from_slice(component);
        if !unique {
            key.extend_from_slice(doc_id);
        }
        Self { key, unique }
    }
}

/// The Index key component for `value` (`None` when the field is missing),
/// or `None` if an Index on a field of Field Type `ty` leaves the value out.
pub(crate) fn key_component(value: Option<&Bson>, ty: FieldType) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    match (value, ty) {
        (None | Some(Bson::Null), _) => keys::push_null(&mut out),
        (Some(Bson::Double(f)), _) if f.is_nan() => return None,
        (Some(Bson::Int32(n)), FieldType::Int) => keys::push_integer(&mut out, i64::from(*n)),
        (Some(Bson::Int64(n)), FieldType::Int) => keys::push_integer(&mut out, *n),
        // Saturates beyond the i64 range; floor keeps the order.
        (Some(Bson::Double(f)), FieldType::Int) => keys::push_integer(&mut out, f.floor() as i64),
        (Some(Bson::Int32(n)), FieldType::Double) => keys::push_double(&mut out, f64::from(*n)),
        // Rounds to the nearest double, which keeps the order.
        (Some(Bson::Int64(n)), FieldType::Double) => keys::push_double(&mut out, *n as f64),
        (Some(Bson::Double(f)), FieldType::Double) => keys::push_double(&mut out, *f),
        // Every pair of numbers was handled above; the other Field Types are
        // Document ID types, encoded as Document IDs are.
        (Some(v), _) if FieldType::of(v) == Some(ty) => {
            keys::encode_id(v, &mut out).expect("every Document ID type encodes")
        }
        _ => return None,
    }
    Some(out)
}

/// A range of keys `find` reads candidate Documents from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CandidateRange {
    /// Index entries, whose values are the encoded IDs of the Documents to
    /// fetch.
    Index(KeyRange),
    /// The Documents themselves, which are keyed by `_id` and so sorted by
    /// it: the Collection is its own `_id` Index.
    Documents(KeyRange),
}

/// The ranges of the Collection with Catalog entry `collection` to read
/// for `filter`, or `None` to scan the whole Collection. Every Document the filter can match is in at least one
/// range; ranges may overlap.
///
/// A comparison can use a range when it must hold for the filter to match
/// (it is at the top level or inside `$and`), its field is `_id` or
/// indexed, and its value is one the field's type compares with. An
/// equality wins, `_id` first; failing that, an `$or` whose every branch
/// can use a range reads the union of the branches' ranges; failing that,
/// the first range comparison is used. Ties go to the first in the filter,
/// and other range comparisons on the chosen field narrow the range. `$ne`
/// never uses a range, and neither does an `$or` with a branch that
/// cannot: that branch's Documents would be missed.
pub(crate) fn plan(filter: &Filter, collection: &CollectionEntry) -> Option<Vec<CandidateRange>> {
    let mut comparisons = Vec::new();
    let mut ors = Vec::new();
    collect_conjuncts(filter, &mut comparisons, &mut ors);
    let candidates: Vec<(&FieldPath, bool, CandidateRange)> = comparisons
        .into_iter()
        .filter_map(|(path, op, value)| {
            let range = if path.as_str() == "_id" {
                let base = collection.prefix();
                CandidateRange::Documents(bounds(base, id_type(value)?, op, value)?)
            } else {
                let index = collection.index_on(path)?;
                let ty = collection.schema.get(&index.path)?.field_type;
                CandidateRange::Index(bounds(collection.index_prefix(index), ty, op, value)?)
            };
            Some((path, op == CompareOp::Eq, range))
        })
        .collect();
    let equality = candidates
        .iter()
        .find(|(path, eq, _)| *eq && path.as_str() == "_id")
        .or_else(|| candidates.iter().find(|(_, eq, _)| *eq));
    if equality.is_none() {
        let union = ors.iter().find_map(|branches| {
            branches.iter().try_fold(Vec::new(), |mut ranges, branch| {
                ranges.extend(plan(branch, collection)?);
                Some(ranges)
            })
        });
        if union.is_some() {
            return union;
        }
    }
    let (chosen, _, mut candidate) = equality.or_else(|| candidates.first())?.clone();
    let (CandidateRange::Index(range) | CandidateRange::Documents(range)) = &mut candidate;
    for (path, _, other) in &candidates {
        if *path == chosen {
            let (CandidateRange::Index(other) | CandidateRange::Documents(other)) = other;
            *range = range.intersect(other);
        }
    }
    Some(vec![candidate])
}

/// The Field Type whose Index keys sort `_id` values the way filters
/// compare them with `value`. Document IDs are encoded as Index keys are,
/// integers as in an Index on an `int` field; `_id` is never a double, so a
/// double filter value works as it does against such an Index.
fn id_type(value: &Bson) -> Option<FieldType> {
    match FieldType::of(value)? {
        FieldType::Double => Some(FieldType::Int),
        ty => Some(ty),
    }
}

/// Every comparison and every `$or` that must hold for `filter` to match:
/// comparisons as (Field path, operator, value), `$or`s as their branches.
fn collect_conjuncts<'f>(
    filter: &'f Filter,
    comparisons: &mut Vec<(&'f FieldPath, CompareOp, &'f Bson)>,
    ors: &mut Vec<&'f [Filter]>,
) {
    match filter {
        Filter::And(fs) => fs
            .iter()
            .for_each(|f| collect_conjuncts(f, comparisons, ors)),
        Filter::Compare { path, op, value } => comparisons.push((path, *op, value)),
        Filter::Or(branches) => ors.push(branches),
    }
}

/// The keys under `base` (an Index's prefix, or a Collection's for `_id`)
/// that can hold a value satisfying `op value`. Bounds are inclusive even
/// for `$gt` and `$lt`, because coercion files different values under one
/// key; the recheck drops the extras.
fn bounds(base: Vec<u8>, ty: FieldType, op: CompareOp, value: &Bson) -> Option<KeyRange> {
    let comp = match value {
        // `{f: null}` matches null and missing, which share one component.
        Bson::Null if op == CompareOp::Eq => key_component(None, ty)?,
        v if ty.compares_with(v) => key_component(Some(v), ty)?,
        _ => return None,
    };
    let at = |bytes: &[u8]| [base.as_slice(), bytes].concat();
    // Every key whose component starts with `comp`, then the whole type.
    let exact = KeyRange::prefix(&at(&comp));
    let whole_type = KeyRange::prefix(&at(&[keys::type_tag(&comp)]));
    let end = |r: KeyRange| r.end.expect("keys under a prefix have an upper bound");
    match op {
        CompareOp::Eq => Some(exact),
        CompareOp::Ne => None,
        _ if ty == FieldType::Binary || value == &Bson::Null => None,
        CompareOp::Gt | CompareOp::Gte => Some(KeyRange::new(exact.start, end(whole_type))),
        CompareOp::Lt | CompareOp::Lte => Some(KeyRange::new(whole_type.start, end(exact))),
    }
}

impl CollectionEntry {
    /// The Index on the field at `path`.
    pub(crate) fn index_on(&self, path: &FieldPath) -> Option<&IndexDef> {
        self.indexes.iter().find(|ix| ix.path == *path)
    }

    /// The prefix of every entry in `index`.
    pub(crate) fn index_prefix(&self, index: &IndexDef) -> Vec<u8> {
        keys::index_prefix(self.namespace_id, self.id, index.id)
    }

    /// The entry the stored Document `doc`, with encoded ID `doc_id`, has in
    /// `index`, or `None` if the Index leaves its value out.
    pub(crate) fn index_entry(
        &self,
        index: &IndexDef,
        doc: &RawDocument,
        doc_id: &[u8],
    ) -> Result<Option<IndexEntry>> {
        let ty = self.schema[&index.path].field_type;
        let value = lookup_raw(doc, &index.path)?;
        Ok(key_component(value.as_ref(), ty)
            .map(|component| IndexEntry::new(self.index_prefix(index), index, &component, doc_id)))
    }
}

impl<E: StorageEngine> Transaction<'_, E> {
    /// Create an Index on a field of an empty Collection, as part of this
    /// Transaction (ADR 0010). There is nothing to build, so it commits or
    /// aborts with the rest of the Transaction.
    ///
    /// Fails with [`Error::CollectionNotEmpty`] if the Collection holds
    /// Documents: use [`Node::create_index`](crate::Node::create_index),
    /// which builds the Index on its own. A Document committed concurrently
    /// makes this Transaction abort with Collection Changed.
    pub fn create_index_on_empty(
        &mut self,
        collection: &CollectionPath,
        path: &FieldPath,
        unique: bool,
    ) -> Result<()> {
        let (mut entry, index) = self.prepare_index(collection, path, unique)?;
        if self.any_live_in(&entry.documents())? {
            return Err(Error::CollectionNotEmpty(collection.clone()));
        }
        self.depend_on_whole_collection(entry.prefix());
        entry.indexes.push(index);
        self.write_collection_entry(&entry)
    }

    /// Check that an Index may be created and give it the Collection's next
    /// Index ID. The caller fills the Index and adds it to the returned
    /// entry, whose counter has moved past the ID.
    pub(crate) fn prepare_index(
        &mut self,
        collection: &CollectionPath,
        path: &FieldPath,
        unique: bool,
    ) -> Result<(CollectionEntry, IndexDef)> {
        let mut entry = self.collection_entry(collection)?;
        let Some(field) = entry.schema.get(path) else {
            return Err(Error::InvalidSchema(format!(
                "declare the type of {path} in the collection schema before indexing it"
            )));
        };
        if entry.index_on(path).is_some() {
            return Err(Error::IndexExists {
                collection: collection.clone(),
                path: path.clone(),
            });
        }
        if unique && !field.strict {
            return Err(Error::InvalidSchema(format!(
                "a unique index needs a strict field; declare {path} strict first"
            )));
        }
        // Skip IDs holding entries from a build that never committed: the
        // counter moves only when the Index is added (ADR 0010).
        let mut index = IndexDef {
            path: path.clone(),
            id: entry.next_index_id,
            unique,
        };
        while mvcc::any_version_under(&self.node.engine, &entry.index_prefix(&index))? {
            index.id += 1;
        }
        entry.next_index_id = index.id + 1;
        Ok((entry, index))
    }

    /// Drop the Index on a field. Its entries become unreachable and stay
    /// behind until garbage collection exists.
    pub fn drop_index(&mut self, collection: &CollectionPath, path: &FieldPath) -> Result<()> {
        let mut entry = self.collection_entry(collection)?;
        let before = entry.indexes.len();
        entry.indexes.retain(|ix| ix.path != *path);
        if entry.indexes.len() == before {
            return Err(Error::NoSuchIndex {
                collection: collection.clone(),
                path: path.clone(),
            });
        }
        self.write_collection_entry(&entry)
    }

    /// The Collection's Indexes, in the order they were created.
    pub fn list_indexes(&mut self, collection: &CollectionPath) -> Result<Vec<IndexInfo>> {
        Ok(self
            .collection_entry(collection)?
            .indexes
            .into_iter()
            .map(|ix| IndexInfo {
                path: ix.path,
                unique: ix.unique,
            })
            .collect())
    }
}

impl<E: StorageEngine> Node<E> {
    /// Create an Index on a field and build it from the Documents already in
    /// the Collection (ADR 0010). The field's type must be declared in the
    /// Collection Schema; a unique Index needs a Strict Field.
    ///
    /// The build runs in the foreground, holding the commit lock, so no
    /// other commit happens until it finishes. It commits the entries in
    /// chunks, each an ordinary fsynced commit, so it never holds more than
    /// one chunk in memory; the entries stay unreachable until a final
    /// commit adds the Index to the Catalog. If the build fails, for
    /// example with [`Error::DuplicateValue`], or the Node crashes, the
    /// committed chunks are left behind under an Index ID that is never
    /// reused in the Collection, until garbage collection exists.
    ///
    /// ```
    /// use bson::doc;
    /// use psdb_core::{Node, FieldSchema, FieldType};
    /// use psdb_storage_memory::MemoryEngine;
    ///
    /// let node = Node::new(MemoryEngine::new())?;
    /// node.create_namespace("shop")?;
    /// let orders = "shop.orders".parse()?;
    /// node.create_collection(&orders)?;
    /// node.insert(&orders, doc! { "total": 30 })?;
    /// let total = FieldSchema { field_type: FieldType::Int, strict: false };
    /// let total_path = "total".parse()?;
    /// node.set_field_type(&orders, &total_path, total)?;
    /// node.create_index(&orders, &total_path, false)?;
    /// assert_eq!(node.find(&orders, &doc! { "total": { "$gt": 10 } })?.len(), 1);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn create_index(
        &self,
        collection: &CollectionPath,
        path: &FieldPath,
        unique: bool,
    ) -> Result<()> {
        let mut commits = self.commit_lock.lock().expect("lock poisoned");
        let mut txn = Transaction::new(self, self.latest_visible_commit_ts());
        let (mut entry, index) = txn.prepare_index(collection, path, unique)?;
        self.build_index(&mut commits, &entry, &index, txn.snapshot_ts())?;
        entry.indexes.push(index);
        txn.write_collection_entry(&entry)?;
        txn.commit_locked(&mut commits)?;
        Ok(())
    }

    /// Commit an entry in `index` for every Document of the Collection at
    /// `snapshot_ts`, one chunk at a time.
    fn build_index(
        &self,
        commits: &mut CommitState,
        entry: &CollectionEntry,
        index: &IndexDef,
        snapshot_ts: u64,
    ) -> Result<()> {
        let prefix_len = entry.prefix().len();
        let mut range = entry.documents();
        loop {
            let mut docs = Vec::new();
            let mut docs_bytes = 0;
            // The first Document of the next chunk, if this one filled up
            // before the Collection ended.
            let mut next_chunk_start = None;
            mvcc::scan_range_at(&self.engine, &range, snapshot_ts, &mut |key, value| {
                let full = docs.len() == BUILD_CHUNK_DOCUMENTS
                    || (!docs.is_empty() && docs_bytes + value.len() > BUILD_CHUNK_BYTES);
                if full {
                    next_chunk_start = Some(key.to_vec());
                    return ControlFlow::Break(());
                }
                docs_bytes += value.len();
                docs.push((key.to_vec(), value));
                ControlFlow::Continue(())
            })?;
            let mut chunk = WriteSet::new();
            for (key, bytes) in &docs {
                let doc_id = &key[prefix_len..];
                let Some(index_entry) = entry.index_entry(index, document::raw(bytes)?, doc_id)?
                else {
                    continue;
                };
                if index_entry.unique
                    && (chunk.contains_key(&index_entry.key)
                        || mvcc::read_at(
                            &self.engine,
                            &index_entry.key,
                            self.latest_visible_commit_ts(),
                        )?
                        .is_some())
                {
                    return Err(Error::DuplicateValue(format!(
                        "two documents share a value of {}, so it cannot have a unique index",
                        index.path
                    )));
                }
                chunk.insert(index_entry.key, Some(doc_id.to_vec()));
            }
            if !chunk.is_empty() {
                // Its keys are under an Index ID nothing else writes, so the
                // conflict checks pass; they run so every commit takes one path.
                Transaction::with_write_set(self, self.latest_visible_commit_ts(), chunk)
                    .commit_locked(commits)?;
            }
            let Some(start) = next_chunk_start else {
                return Ok(());
            };
            // The newest Version of a key sorts first, at timestamp u64::MAX.
            range.start = keys::physical_key(&start, u64::MAX);
        }
    }
}

#[cfg(test)]
mod tests {
    use bson::doc;

    use super::*;
    use crate::schema::{FieldSchema, Schema};

    fn schema(fields: &[(&str, FieldType)]) -> Schema {
        fields
            .iter()
            .map(|&(path, field_type)| {
                let field = FieldSchema {
                    field_type,
                    strict: false,
                };
                (path.parse().unwrap(), field)
            })
            .collect()
    }

    fn index(path: &str, id: u32) -> IndexDef {
        IndexDef {
            path: path.parse().unwrap(),
            id,
            unique: false,
        }
    }

    fn planned(
        filter: bson::Document,
        schema: &Schema,
        indexes: &[IndexDef],
    ) -> Option<Vec<CandidateRange>> {
        let collection = CollectionEntry {
            namespace_id: 1,
            name: "c".into(),
            id: 3,
            schema: schema.clone(),
            indexes: indexes.to_vec(),
            next_index_id: 0,
        };
        plan(&Filter::parse(&filter).unwrap(), &collection)
    }

    fn range(candidate: &CandidateRange) -> &KeyRange {
        let (CandidateRange::Index(r) | CandidateRange::Documents(r)) = candidate;
        r
    }

    fn comp(v: Bson, ty: FieldType) -> Vec<u8> {
        key_component(Some(&v), ty).unwrap()
    }

    #[test]
    fn number_indexes_coerce_the_other_kind() {
        use FieldType::*;
        assert_eq!(comp(Bson::Double(30.5), Int), comp(Bson::Int32(30), Int));
        assert_eq!(
            comp(Bson::Double(-0.5), Int),
            comp(Bson::Int32(-1), Int),
            "floor, not truncate"
        );
        assert_eq!(
            comp(Bson::Int64(3), Double),
            comp(Bson::Double(3.0), Double)
        );
        assert_eq!(
            comp(Bson::Double(1e300), Int),
            comp(Bson::Int64(i64::MAX), Int),
            "saturates"
        );
        assert_eq!(key_component(Some(&Bson::Double(f64::NAN)), Double), None);
        assert_eq!(
            key_component(Some(&Bson::String("30".into())), Int),
            None,
            "strings are not numbers"
        );
        assert_eq!(key_component(Some(&Bson::Array(vec![])), Int), None);
        assert_eq!(
            key_component(None, Int),
            key_component(Some(&Bson::Null), String)
        );
    }

    #[test]
    fn plan_prefers_equality_and_narrows_ranges() {
        let s = schema(&[("age", FieldType::Int), ("status", FieldType::String)]);
        let ix = [index("age", 7), index("status", 9)];
        let status_range = planned(doc! { "status": "open" }, &s, &ix).unwrap();
        assert_eq!(
            planned(doc! { "age": { "$gte": 18 }, "status": "open" }, &s, &ix),
            Some(status_range),
            "equality wins"
        );
        let adults = planned(doc! { "age": { "$gte": 18 } }, &s, &ix).unwrap();
        let working = planned(doc! { "age": { "$gte": 18, "$lt": 65 } }, &s, &ix).unwrap();
        let (adults, working) = (range(&adults[0]), range(&working[0]));
        assert_eq!(working.start, adults.start);
        assert!(
            working.end < adults.end,
            "the second bound narrows the range"
        );
    }

    #[test]
    fn plan_falls_back_to_a_scan() {
        let s = schema(&[("age", FieldType::Int)]);
        let ix = [index("age", 7)];
        for filter in [
            doc! { "age": { "$ne": 3 } },
            doc! { "$or": [ { "age": 3 }, { "name": "x" } ] },
            doc! { "age": "thirty" },
            doc! { "age": { "$gt": null } },
            doc! { "age": f64::NAN },
            doc! { "name": "x" },
        ] {
            assert_eq!(planned(filter.clone(), &s, &ix), None, "{filter}");
        }
        assert!(
            planned(doc! { "age": null }, &s, &ix).is_some(),
            "null and missing are indexed"
        );
        assert!(planned(doc! { "$and": [ { "age": 3 } ] }, &s, &ix).is_some());
    }

    #[test]
    fn or_reads_the_union_when_every_branch_is_indexed() {
        let s = schema(&[("age", FieldType::Int), ("status", FieldType::String)]);
        let ix = [index("age", 7), index("status", 9)];
        let either = doc! { "$or": [ { "status": "open" }, { "age": { "$gt": 65 } } ] };
        assert_eq!(planned(either, &s, &ix).map(|r| r.len()), Some(2));
        let nested =
            doc! { "$or": [ { "status": "a" }, { "$or": [ { "status": "b" }, { "age": 1 } ] } ] };
        assert_eq!(planned(nested, &s, &ix).map(|r| r.len()), Some(3));
        assert_eq!(
            planned(
                doc! { "age": { "$gt": 1 }, "$or": [ { "status": "a" }, { "status": "b" } ] },
                &s,
                &ix
            )
            .map(|r| r.len()),
            Some(2),
            "an indexed $or beats a range"
        );
        assert_eq!(
            planned(
                doc! { "age": 1, "$or": [ { "status": "a" }, { "status": "b" } ] },
                &s,
                &ix
            )
            .map(|r| r.len()),
            Some(1),
            "an equality beats an $or"
        );
        let open = planned(doc! { "status": "open" }, &s, &ix);
        assert_eq!(
            planned(
                doc! { "status": "open", "$or": [ { "status": "a" }, { "name": "x" } ] },
                &s,
                &ix
            ),
            open,
            "an $or with an unindexed branch is skipped, not fatal"
        );
    }

    #[test]
    fn id_comparisons_read_documents_directly() {
        let s = schema(&[("age", FieldType::Int)]);
        let ix = [index("age", 7)];
        let docs = |filter| match planned(filter, &s, &ix).as_deref() {
            Some([CandidateRange::Documents(r)]) => Some(r.clone()),
            _ => None,
        };
        let one = docs(doc! { "_id": 5 }).unwrap();
        assert!(one.contains(&keys::document_key(1, 3, &Bson::Int32(5)).unwrap()));
        assert!(!one.contains(&keys::document_key(1, 3, &Bson::Int32(6)).unwrap()));
        assert_eq!(
            docs(doc! { "age": 1, "_id": 5 }),
            Some(one),
            "an _id equality beats an Index equality"
        );
        let below = docs(doc! { "_id": { "$lt": -2 } }).unwrap();
        assert!(below.contains(&keys::document_key(1, 3, &Bson::Int64(-7)).unwrap()));
        assert!(!below.contains(&keys::document_key(1, 3, &Bson::Int64(3)).unwrap()));
        assert!(
            docs(doc! { "_id": { "$gt": 2.5 } })
                .unwrap()
                .contains(&keys::document_key(1, 3, &Bson::Int32(3)).unwrap()),
            "a double bound works on integer IDs"
        );
        assert_eq!(
            planned(doc! { "$or": [ { "_id": 1 }, { "age": 2 } ] }, &s, &ix).map(|r| r.len()),
            Some(2)
        );
        assert_eq!(planned(doc! { "_id": null }, &s, &ix), None);
    }

    #[test]
    fn binary_indexes_answer_equality_only() {
        let s = schema(&[("b", FieldType::Binary)]);
        let ix = [index("b", 7)];
        let bin = Bson::Binary(bson::Binary {
            subtype: bson::spec::BinarySubtype::Generic,
            bytes: vec![1],
        });
        assert!(planned(doc! { "b": bin.clone() }, &s, &ix).is_some());
        assert_eq!(planned(doc! { "b": { "$gt": bin } }, &s, &ix), None);
    }
}
