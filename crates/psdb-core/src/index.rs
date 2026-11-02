//! Indexes: Index Types, Strict Fields, Index entry keys, and choosing an
//! Index for a filter (ADR 0010, ADR 0011).
//!
//! An Index entry's logical key is `database_id ‖ index_id ‖ component ‖
//! document_id`, where the component encodes the Document's value in the
//! indexed field (see [`crate::keys`]). A unique Index leaves the Document
//! ID out, so two Transactions writing the same value write the same key
//! and the second to commit gets a Write Conflict; null and missing keep
//! it, because they never collide. The value of every entry is the encoded
//! Document ID, so a lookup can fetch the Document.
//!
//! Entries point to Document IDs, never to where a Version is stored, so a
//! write that leaves the indexed value unchanged writes nothing to the
//! Index. A lookup always fetches the Document at its Snapshot and rechecks
//! the whole filter, which also makes coerced keys safe: an `int` Index
//! files `30.5` under `30`, and range scans include their bounds.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use bson::{Bson, RawDocument};
use psdb_storage::KeyRange;

use crate::error::{Error, Result};
use crate::filter::{CompareOp, Filter, lookup_raw};
use crate::keys;

/// The kind of value an Index covers, declared per field in the Collection
/// Schema.
///
/// Values of other types are left out of the Index. The two number types
/// coerce the other kind of number into the key instead, because filters
/// compare integers and doubles as numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexType {
    /// Integers; doubles are filed under their floor.
    Int,
    /// Doubles; integers are filed as the nearest double.
    Double,
    /// Strings.
    String,
    /// BSON dates.
    Date,
    /// Booleans.
    Bool,
    /// ObjectIds.
    ObjectId,
    /// Binary values. Filters only test binary for equality, so the Index
    /// only answers equality.
    Binary,
}

impl IndexType {
    /// Every Index Type, in the order of [`IndexType::name`]'s list.
    pub const ALL: [IndexType; 7] = [
        IndexType::Int,
        IndexType::Double,
        IndexType::String,
        IndexType::Date,
        IndexType::Bool,
        IndexType::ObjectId,
        IndexType::Binary,
    ];

    /// The name used in the Catalog and by clients: `int`, `double`,
    /// `string`, `date`, `bool`, `objectId` or `binary`.
    pub fn name(self) -> &'static str {
        match self {
            IndexType::Int => "int",
            IndexType::Double => "double",
            IndexType::String => "string",
            IndexType::Date => "date",
            IndexType::Bool => "bool",
            IndexType::ObjectId => "objectId",
            IndexType::Binary => "binary",
        }
    }

    /// Whether a filter value can be compared with values of this type, so
    /// an Index of this type can narrow a comparison against it.
    fn compares_with(self, value: &Bson) -> bool {
        matches!(
            (self, value),
            (
                IndexType::Int | IndexType::Double,
                Bson::Int32(_) | Bson::Int64(_) | Bson::Double(_)
            ) | (IndexType::String, Bson::String(_))
                | (IndexType::Date, Bson::DateTime(_))
                | (IndexType::Bool, Bson::Boolean(_))
                | (IndexType::ObjectId, Bson::ObjectId(_))
                | (IndexType::Binary, Bson::Binary(_))
        )
    }
}

impl fmt::Display for IndexType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for IndexType {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        IndexType::ALL
            .into_iter()
            .find(|t| t.name() == s)
            .ok_or_else(|| {
                Error::InvalidSchema(format!(
                    "unknown Index Type {s:?}; use int, double, string, date, bool, objectId or binary"
                ))
            })
    }
}

/// A field's declaration in a Collection Schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldType {
    /// The Index Type of the field.
    pub index_type: IndexType,
    /// Whether this is a Strict Field: writes giving it a value of another
    /// type fail with [`Error::TypeMismatch`]. Missing and null are always
    /// allowed.
    pub strict: bool,
}

/// An Index as clients see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexInfo {
    /// The indexed field path, such as `address.city`.
    pub path: String,
    /// Whether no two Documents may share a value in the field.
    pub unique: bool,
}

/// An Index as the Catalog stores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexDef {
    pub(crate) path: String,
    /// Drawn from the same never-reused ID space as Collections (ADR 0006).
    pub(crate) id: u32,
    pub(crate) unique: bool,
}

/// A Collection Schema: field path → its declaration.
pub(crate) type Schema = BTreeMap<String, FieldType>;

/// Split and check a field path for the Collection Schema: non-empty
/// segments, none starting with `$`, and not `_id`, which is already the
/// key of every Document.
pub(crate) fn split_path(path: &str) -> Result<Vec<String>> {
    let segments: Vec<String> = path.split('.').map(str::to_owned).collect();
    if segments.iter().any(|s| s.is_empty() || s.starts_with('$')) {
        return Err(Error::InvalidSchema(format!(
            "{path:?} is not a field path"
        )));
    }
    if segments[0] == "_id" {
        return Err(Error::InvalidSchema(
            "_id cannot be declared or indexed; Documents are already keyed by it".into(),
        ));
    }
    Ok(segments)
}

/// The Index key component for `value` (`None` when the field is missing),
/// or `None` if the value is left out of an Index of type `ty`.
pub(crate) fn component(value: Option<&Bson>, ty: IndexType) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    match (value, ty) {
        (None | Some(Bson::Null), _) => keys::push_null(&mut out),
        (Some(Bson::Double(f)), _) if f.is_nan() => return None,
        (Some(Bson::Int32(n)), IndexType::Int) => keys::push_integer(&mut out, i64::from(*n)),
        (Some(Bson::Int64(n)), IndexType::Int) => keys::push_integer(&mut out, *n),
        // Saturates beyond the i64 range; floor keeps the order.
        (Some(Bson::Double(f)), IndexType::Int) => keys::push_integer(&mut out, f.floor() as i64),
        (Some(Bson::Int32(n)), IndexType::Double) => keys::push_double(&mut out, f64::from(*n)),
        // Rounds to the nearest double, which keeps the order.
        (Some(Bson::Int64(n)), IndexType::Double) => keys::push_double(&mut out, *n as f64),
        (Some(Bson::Double(f)), IndexType::Double) => keys::push_double(&mut out, *f),
        (Some(v @ Bson::String(_)), IndexType::String)
        | (Some(v @ Bson::DateTime(_)), IndexType::Date)
        | (Some(v @ Bson::Boolean(_)), IndexType::Bool)
        | (Some(v @ Bson::ObjectId(_)), IndexType::ObjectId)
        | (Some(v @ Bson::Binary(_)), IndexType::Binary) => {
            keys::encode_id(v, &mut out).expect("every Document ID type encodes")
        }
        _ => return None,
    }
    Some(out)
}

/// The Index key component for the value at `path` in a stored Document.
pub(crate) fn component_of(
    doc: &RawDocument,
    path: &str,
    ty: IndexType,
) -> Result<Option<Vec<u8>>> {
    let segments: Vec<String> = path.split('.').map(str::to_owned).collect();
    Ok(component(lookup_raw(doc, &segments)?.as_ref(), ty))
}

/// Fail with [`Error::TypeMismatch`] if `doc` breaks any Strict Field in
/// `schema`.
pub(crate) fn check_strict(doc: &RawDocument, schema: &Schema) -> Result<()> {
    for (path, field) in schema.iter().filter(|(_, f)| f.strict) {
        check_field(doc, path, *field)?;
    }
    Ok(())
}

/// Fail with [`Error::TypeMismatch`] unless the value at `path` is missing,
/// null, or converts exactly to the field's Index Type.
pub(crate) fn check_field(doc: &RawDocument, path: &str, field: FieldType) -> Result<()> {
    let segments: Vec<String> = path.split('.').map(str::to_owned).collect();
    match lookup_raw(doc, &segments)? {
        None | Some(Bson::Null) => Ok(()),
        Some(v) if fits_exactly(&v, field.index_type) => Ok(()),
        Some(v) => Err(Error::TypeMismatch(format!(
            "{path} is a strict {} field, but this document has {v}",
            field.index_type
        ))),
    }
}

/// Whether `value` belongs in a Strict Field of type `ty` without losing
/// anything: an `int` accepts `30.0` (which JavaScript sends for `30`) but
/// not `30.5`; a `double` accepts integers up to 2⁵³ but not NaN.
fn fits_exactly(value: &Bson, ty: IndexType) -> bool {
    // 2⁶³ is an exact double; every i64 lies in [-2⁶³, 2⁶³).
    const TWO_63: f64 = 9_223_372_036_854_775_808.0;
    const TWO_53: u64 = 1 << 53;
    match (value, ty) {
        (Bson::Int32(_) | Bson::Int64(_), IndexType::Int) => true,
        (Bson::Double(f), IndexType::Int) => f.fract() == 0.0 && *f >= -TWO_63 && *f < TWO_63,
        (Bson::Int32(_), IndexType::Double) => true,
        (Bson::Int64(n), IndexType::Double) => n.unsigned_abs() <= TWO_53,
        (Bson::Double(f), IndexType::Double) => !f.is_nan(),
        (v, ty) => component(Some(v), ty).is_some(),
    }
}

/// The logical key of the entry for the Document with encoded ID `doc_id`
/// under `component`.
pub(crate) fn entry_key(
    database_id: u32,
    index: &IndexDef,
    component: &[u8],
    doc_id: &[u8],
) -> Vec<u8> {
    let mut key = keys::collection_prefix(database_id, index.id);
    key.extend_from_slice(component);
    if !index.unique || keys::is_null(component) {
        key.extend_from_slice(doc_id);
    }
    key
}

/// The ranges of Index entries to read for `filter`, or `None` to scan the
/// whole Collection. Every Document the filter can match has an entry in
/// at least one range; ranges may overlap.
///
/// A comparison can use an Index when it must hold for the filter to match
/// (it is at the top level or inside `$and`), its field is indexed, and its
/// value is one the Index's type compares with. An equality wins; failing
/// that, an `$or` whose every branch can use an Index reads the union of
/// the branches' ranges; failing that, the first range comparison is used.
/// Ties go to the first in the filter, and other range comparisons on the
/// chosen field narrow the range. `$ne` never uses an Index, and neither
/// does an `$or` with a branch that cannot: that branch's Documents would
/// be missed.
pub(crate) fn plan(
    filter: &Filter,
    database_id: u32,
    schema: &Schema,
    indexes: &[IndexDef],
) -> Option<Vec<KeyRange>> {
    let mut comparisons = Vec::new();
    let mut ors = Vec::new();
    collect_conjuncts(filter, &mut comparisons, &mut ors);
    let candidates: Vec<(&IndexDef, bool, KeyRange)> = comparisons
        .into_iter()
        .filter_map(|(path, op, value)| {
            let index = indexes.iter().find(|i| i.path == path)?;
            let ty = schema.get(&index.path)?.index_type;
            let range = bounds(database_id, index, ty, op, value)?;
            Some((index, op == CompareOp::Eq, range))
        })
        .collect();
    let equality = candidates.iter().find(|(_, eq, _)| *eq);
    if equality.is_none() {
        let union = ors.iter().find_map(|branches| {
            branches.iter().try_fold(Vec::new(), |mut ranges, branch| {
                ranges.extend(plan(branch, database_id, schema, indexes)?);
                Some(ranges)
            })
        });
        if union.is_some() {
            return union;
        }
    }
    let (chosen, _, mut range) = equality.or_else(|| candidates.first())?.clone();
    for (index, _, other) in &candidates {
        if index.id == chosen.id {
            if other.start > range.start {
                range.start = other.start.clone();
            }
            if other.end < range.end {
                range.end = other.end.clone();
            }
        }
    }
    Some(vec![range])
}

/// Every comparison and every `$or` that must hold for `filter` to match:
/// comparisons as (dotted path, operator, value), `$or`s as their branches.
fn collect_conjuncts<'f>(
    filter: &'f Filter,
    comparisons: &mut Vec<(String, CompareOp, &'f Bson)>,
    ors: &mut Vec<&'f [Filter]>,
) {
    match filter {
        Filter::And(fs) => fs
            .iter()
            .for_each(|f| collect_conjuncts(f, comparisons, ors)),
        Filter::Compare { path, op, value } => comparisons.push((path.join("."), *op, value)),
        Filter::Or(branches) => ors.push(branches),
    }
}

/// The entries of `index` that can hold a value satisfying `op value`.
/// Bounds are inclusive even for `$gt` and `$lt`, because coercion files
/// different values under one key; the recheck drops the extras.
fn bounds(
    database_id: u32,
    index: &IndexDef,
    ty: IndexType,
    op: CompareOp,
    value: &Bson,
) -> Option<KeyRange> {
    let comp = match value {
        // `{f: null}` matches null and missing, which share one component.
        Bson::Null if op == CompareOp::Eq => component(None, ty)?,
        v if ty.compares_with(v) => component(Some(v), ty)?,
        _ => return None,
    };
    let base = keys::collection_prefix(database_id, index.id);
    let at = |bytes: &[u8]| [base.as_slice(), bytes].concat();
    // Every key whose component starts with `comp`, then the whole type.
    let exact = KeyRange::prefix(&at(&comp));
    let whole_type = KeyRange::prefix(&at(&[keys::type_tag(&comp)]));
    let end = |r: KeyRange| r.end.expect("keys under a prefix have an upper bound");
    match op {
        CompareOp::Eq => Some(exact),
        CompareOp::Ne => None,
        _ if ty == IndexType::Binary || value == &Bson::Null => None,
        CompareOp::Gt | CompareOp::Gte => Some(KeyRange::new(exact.start, end(whole_type))),
        CompareOp::Lt | CompareOp::Lte => Some(KeyRange::new(whole_type.start, end(exact))),
    }
}

#[cfg(test)]
mod tests {
    use bson::doc;

    use super::*;

    fn schema(path: &str, index_type: IndexType) -> Schema {
        Schema::from([(
            path.to_owned(),
            FieldType {
                index_type,
                strict: false,
            },
        )])
    }

    fn index(path: &str, id: u32) -> IndexDef {
        IndexDef {
            path: path.into(),
            id,
            unique: false,
        }
    }

    fn planned(
        filter: bson::Document,
        schema: &Schema,
        indexes: &[IndexDef],
    ) -> Option<Vec<KeyRange>> {
        plan(&Filter::parse(&filter).unwrap(), 1, schema, indexes)
    }

    fn comp(v: Bson, ty: IndexType) -> Vec<u8> {
        component(Some(&v), ty).unwrap()
    }

    #[test]
    fn number_indexes_coerce_the_other_kind() {
        use IndexType::*;
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
        assert_eq!(component(Some(&Bson::Double(f64::NAN)), Double), None);
        assert_eq!(
            component(Some(&Bson::String("30".into())), Int),
            None,
            "strings are not numbers"
        );
        assert_eq!(component(Some(&Bson::Array(vec![])), Int), None);
        assert_eq!(component(None, Int), component(Some(&Bson::Null), String));
    }

    #[test]
    fn strict_fields_accept_only_exact_values() {
        use IndexType::*;
        assert!(fits_exactly(&Bson::Double(30.0), Int));
        assert!(!fits_exactly(&Bson::Double(30.5), Int));
        assert!(!fits_exactly(&Bson::Double(1e300), Int));
        assert!(fits_exactly(&Bson::Int64(1 << 53), Double));
        assert!(!fits_exactly(&Bson::Int64((1 << 53) + 1), Double));
        assert!(!fits_exactly(&Bson::Double(f64::NAN), Double));
        assert!(fits_exactly(&Bson::Double(f64::INFINITY), Double));
        assert!(!fits_exactly(&Bson::String("x".into()), Int));
        assert!(fits_exactly(&Bson::String("x".into()), String));
    }

    #[test]
    fn plan_prefers_equality_and_narrows_ranges() {
        let s = Schema::from([
            (
                "age".to_owned(),
                FieldType {
                    index_type: IndexType::Int,
                    strict: false,
                },
            ),
            (
                "status".to_owned(),
                FieldType {
                    index_type: IndexType::String,
                    strict: false,
                },
            ),
        ]);
        let ix = [index("age", 7), index("status", 9)];
        let status_range = planned(doc! { "status": "open" }, &s, &ix).unwrap();
        assert_eq!(
            planned(doc! { "age": { "$gte": 18 }, "status": "open" }, &s, &ix),
            Some(status_range),
            "equality wins"
        );
        let adults = &planned(doc! { "age": { "$gte": 18 } }, &s, &ix).unwrap()[0];
        let working = &planned(doc! { "age": { "$gte": 18, "$lt": 65 } }, &s, &ix).unwrap()[0];
        assert_eq!(working.start, adults.start);
        assert!(
            working.end < adults.end,
            "the second bound narrows the range"
        );
    }

    #[test]
    fn plan_falls_back_to_a_scan() {
        let s = schema("age", IndexType::Int);
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
        let s = Schema::from([
            (
                "age".to_owned(),
                FieldType {
                    index_type: IndexType::Int,
                    strict: false,
                },
            ),
            (
                "status".to_owned(),
                FieldType {
                    index_type: IndexType::String,
                    strict: false,
                },
            ),
        ]);
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
    fn binary_indexes_answer_equality_only() {
        let s = schema("b", IndexType::Binary);
        let ix = [index("b", 7)];
        let bin = Bson::Binary(bson::Binary {
            subtype: bson::spec::BinarySubtype::Generic,
            bytes: vec![1],
        });
        assert!(planned(doc! { "b": bin.clone() }, &s, &ix).is_some());
        assert_eq!(planned(doc! { "b": { "$gt": bin } }, &s, &ix), None);
    }

    #[test]
    fn paths() {
        assert_eq!(split_path("a.b").unwrap(), ["a", "b"]);
        for bad in ["", "a..b", "$a", "a.$b", "_id", "_id.x"] {
            assert!(
                matches!(split_path(bad), Err(Error::InvalidSchema(_))),
                "{bad:?}"
            );
        }
    }
}
