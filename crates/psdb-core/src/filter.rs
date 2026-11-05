//! Query filters for scanning a Collection.
//!
//! Supported in v0.1, in MongoDB syntax:
//!
//! - equality: `{ "status": "open" }`
//! - comparison: `{ "age": { "$gte": 18, "$lt": 65 } }` with `$eq`, `$ne`,
//!   `$gt`, `$gte`, `$lt`, `$lte`
//! - `$and` and `$or` over a list of filters
//! - dotted paths into nested documents: `{ "address.city": "Paris" }`
//!
//! Not yet supported: matching elements inside arrays, array indexes in
//! paths, `$in`, `$exists` and other operators.

use std::cmp::Ordering;

use bson::{Bson, Document, RawDocument, RawDocumentBuf};

use crate::document::lookup_raw;
use crate::error::{Error, Result};
use crate::field_path::FieldPath;

/// A parsed filter.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Filter {
    /// Every sub-filter matches. An empty `And` matches everything.
    And(Vec<Filter>),
    /// At least one sub-filter matches.
    Or(Vec<Filter>),
    /// The value at `path` compares to `value` as `op` requires.
    Compare {
        /// The field compared.
        path: FieldPath,
        /// The comparison.
        op: CompareOp,
        /// The value compared against.
        value: Bson,
    },
}

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    /// `$eq`
    Eq,
    /// `$ne`
    Ne,
    /// `$gt`
    Gt,
    /// `$gte`
    Gte,
    /// `$lt`
    Lt,
    /// `$lte`
    Lte,
}

impl Filter {
    /// Parse a filter document.
    ///
    /// ```
    /// use bson::doc;
    /// use psdb_core::Filter;
    ///
    /// let filter = Filter::parse(&doc! { "age": { "$gte": 18 }, "address.city": "Paris" })?;
    /// assert!(filter.matches(&doc! { "age": 30, "address": { "city": "Paris" } })?);
    /// assert!(!filter.matches(&doc! { "age": 12, "address": { "city": "Paris" } })?);
    /// # Ok::<(), psdb_core::Error>(())
    /// ```
    pub fn parse(filter: &Document) -> Result<Filter> {
        let mut clauses = Vec::new();
        for (key, value) in filter {
            match key.as_str() {
                "$and" => clauses.push(Filter::And(parse_list(key, value)?)),
                "$or" => clauses.push(Filter::Or(parse_list(key, value)?)),
                op if op.starts_with('$') => {
                    return Err(Error::InvalidFilter(format!(
                        "unsupported top-level operator {op}"
                    )));
                }
                path => clauses.extend(parse_field(path, value)?),
            }
        }
        Ok(if clauses.len() == 1 {
            clauses.remove(0)
        } else {
            Filter::And(clauses)
        })
    }

    /// Whether `doc` matches this filter. Fails with
    /// [`Error::InvalidDocument`] if `doc` cannot be encoded as BSON.
    pub fn matches(&self, doc: &Document) -> Result<bool> {
        let raw = RawDocumentBuf::try_from(doc)
            .map_err(|e| Error::InvalidDocument(format!("cannot encode as BSON: {e}")))?;
        self.matches_raw(&raw)
    }

    /// Whether a stored Document matches, reading only the fields the
    /// filter names instead of decoding the whole Document.
    pub(crate) fn matches_raw(&self, doc: &RawDocument) -> Result<bool> {
        Ok(match self {
            Filter::And(fs) => {
                for f in fs {
                    if !f.matches_raw(doc)? {
                        return Ok(false);
                    }
                }
                true
            }
            Filter::Or(fs) => {
                for f in fs {
                    if f.matches_raw(doc)? {
                        return Ok(true);
                    }
                }
                false
            }
            Filter::Compare { path, op, value } => {
                compare_at(lookup_raw(doc, path)?.as_ref(), *op, value)
            }
        })
    }
}

fn parse_list(op: &str, value: &Bson) -> Result<Vec<Filter>> {
    let Bson::Array(items) = value else {
        return Err(Error::InvalidFilter(format!(
            "{op} takes an array of filters"
        )));
    };
    if items.is_empty() {
        return Err(Error::InvalidFilter(format!(
            "{op} needs at least one filter"
        )));
    }
    items
        .iter()
        .map(|item| match item {
            Bson::Document(d) => Filter::parse(d),
            _ => Err(Error::InvalidFilter(format!(
                "{op} takes an array of filters"
            ))),
        })
        .collect()
}

fn parse_field(path: &str, value: &Bson) -> Result<Vec<Filter>> {
    let path: FieldPath = path
        .parse()
        .map_err(|_| Error::InvalidFilter(format!("{path:?} is not a field path")))?;
    let compare = |op, value: &Bson| Filter::Compare {
        path: path.clone(),
        op,
        value: value.clone(),
    };

    // `{ field: { $op: v, ... } }` is a set of comparisons; any other value is
    // an equality match, including a document whose keys are not operators.
    let operators = match value {
        Bson::Document(d) if d.keys().next().is_some_and(|k| k.starts_with('$')) => d,
        _ => return Ok(vec![compare(CompareOp::Eq, value)]),
    };
    operators
        .iter()
        .map(|(op, v)| {
            let op = match op.as_str() {
                "$eq" => CompareOp::Eq,
                "$ne" => CompareOp::Ne,
                "$gt" => CompareOp::Gt,
                "$gte" => CompareOp::Gte,
                "$lt" => CompareOp::Lt,
                "$lte" => CompareOp::Lte,
                other => {
                    return Err(Error::InvalidFilter(format!(
                        "unsupported operator {other}"
                    )));
                }
            };
            Ok(compare(op, v))
        })
        .collect()
}

fn compare_at(found: Option<&Bson>, op: CompareOp, target: &Bson) -> bool {
    // As in MongoDB, a missing field equals null.
    let equal = match found {
        Some(v) => compare(v, target) == Some(Ordering::Equal),
        None => matches!(target, Bson::Null),
    };
    match op {
        CompareOp::Eq => equal,
        CompareOp::Ne => !equal,
        _ => {
            let Some(ordering) = found.and_then(|v| compare(v, target)) else {
                return false;
            };
            match op {
                CompareOp::Gt => ordering.is_gt(),
                CompareOp::Gte => ordering.is_ge(),
                CompareOp::Lt => ordering.is_lt(),
                CompareOp::Lte => ordering.is_le(),
                CompareOp::Eq | CompareOp::Ne => unreachable!(),
            }
        }
    }
}

/// Order two values of the same comparable type. Numbers compare across
/// int32, int64 and double, exactly: an integer is never rounded to a
/// double. NaN equals nothing, itself included. Documents, arrays and
/// binary only compare for equality. Values of different types are
/// incomparable, so range operators never match across types.
fn compare(a: &Bson, b: &Bson) -> Option<Ordering> {
    use Bson::*;
    match (a, b) {
        (Int32(_) | Int64(_), Int32(_) | Int64(_)) => Some(as_i64(a)?.cmp(&as_i64(b)?)),
        (Int32(_) | Int64(_), Double(f)) => cmp_int_double(as_i64(a)?, *f),
        (Double(f), Int32(_) | Int64(_)) => Some(cmp_int_double(as_i64(b)?, *f)?.reverse()),
        (Double(x), Double(y)) => x.partial_cmp(y),
        (String(x), String(y)) => Some(x.as_bytes().cmp(y.as_bytes())),
        (Boolean(x), Boolean(y)) => Some(x.cmp(y)),
        (DateTime(x), DateTime(y)) => Some(x.cmp(y)),
        (ObjectId(x), ObjectId(y)) => Some(x.bytes().cmp(&y.bytes())),
        (Null, Null) => Some(Ordering::Equal),
        _ if a == b => Some(Ordering::Equal),
        _ => None,
    }
}

fn as_i64(v: &Bson) -> Option<i64> {
    match v {
        Bson::Int32(n) => Some(i64::from(*n)),
        Bson::Int64(n) => Some(*n),
        _ => None,
    }
}

/// 2⁶³ as a double, which is exact. Every i64 lies in [-2⁶³, 2⁶³), so a
/// double in that range has a whole part that converts to i64 exactly.
pub(crate) const TWO_63: f64 = 9_223_372_036_854_775_808.0;

/// Compare an integer with a double without converting the integer, which
/// would round integers above 2⁵³ and make equality non-transitive.
fn cmp_int_double(i: i64, f: f64) -> Option<Ordering> {
    if f.is_nan() {
        return None;
    }
    if f >= TWO_63 {
        return Some(Ordering::Less);
    }
    if f < -TWO_63 {
        return Some(Ordering::Greater);
    }
    // In range, so the whole part converts to i64 exactly.
    let whole = f.trunc();
    Some(i.cmp(&(whole as i64)).then_with(|| {
        // Same whole part: the fraction decides.
        0.0f64.partial_cmp(&(f - whole)).expect("finite")
    }))
}

#[cfg(test)]
mod tests {
    use bson::doc;
    use proptest::prelude::*;

    use super::*;

    fn matches(filter: Document, doc: Document) -> bool {
        Filter::parse(&filter).unwrap().matches(&doc).unwrap()
    }

    #[test]
    fn equality_and_missing_fields() {
        assert!(matches(doc! { "a": 1 }, doc! { "a": 1 }));
        assert!(
            matches(doc! { "a": 1 }, doc! { "a": 1.0 }),
            "numbers compare across types"
        );
        assert!(!matches(doc! { "a": 1 }, doc! { "a": "1" }));
        assert!(
            matches(doc! { "a": null }, doc! { "b": 1 }),
            "missing equals null"
        );
        assert!(
            matches(doc! { "a": { "$ne": 1 } }, doc! { "b": 1 }),
            "missing is not equal to 1"
        );
        assert!(
            matches(doc! {}, doc! { "x": 1 }),
            "empty filter matches everything"
        );
    }

    #[test]
    fn integers_and_doubles_compare_exactly() {
        let big = 9_007_199_254_740_993i64; // 2⁵³ + 1, not a double
        let near = 9_007_199_254_740_992.0f64; // 2⁵³
        assert!(
            !matches(doc! { "a": near }, doc! { "a": big }),
            "2⁵³ + 1 is not 2⁵³"
        );
        assert!(matches(doc! { "a": { "$gt": near } }, doc! { "a": big }));
        assert!(matches(doc! { "a": near }, doc! { "a": big - 1 }));
        assert!(matches(doc! { "a": { "$lt": 3.5 } }, doc! { "a": 3 }));
        assert!(matches(doc! { "a": { "$gt": -3.5 } }, doc! { "a": -3 }));
        assert!(matches(doc! { "a": { "$lt": -3.5 } }, doc! { "a": -4 }));
        assert!(matches(doc! { "a": -0.0 }, doc! { "a": 0 }));
        assert!(matches(
            doc! { "a": { "$lt": 1e300 } },
            doc! { "a": i64::MAX }
        ));
        assert!(matches(
            doc! { "a": { "$gt": f64::NEG_INFINITY } },
            doc! { "a": i64::MIN }
        ));
        assert!(
            !matches(doc! { "a": f64::NAN }, doc! { "a": f64::NAN }),
            "NaN equals nothing"
        );
        assert!(!matches(doc! { "a": { "$lt": f64::NAN } }, doc! { "a": 1 }));
    }

    proptest! {
        #[test]
        fn int_double_order_agrees_with_exact_arithmetic(i: i64, f in prop::num::f64::NORMAL | prop::num::f64::ZERO) {
            // An i128 holds every i64 and every integral double in range exactly.
            let exact = if f.fract() == 0.0 && f.abs() < 1e38 {
                Some((i as i128).cmp(&(f as i128)))
            } else {
                None
            };
            if let Some(want) = exact {
                prop_assert_eq!(cmp_int_double(i, f), Some(want));
            }
        }
    }

    #[test]
    fn range_operators_stay_within_a_type() {
        let adult = doc! { "age": { "$gte": 18, "$lt": 65 } };
        assert!(matches(adult.clone(), doc! { "age": 18 }));
        assert!(!matches(adult.clone(), doc! { "age": 65 }));
        assert!(
            !matches(adult.clone(), doc! { "age": "30" }),
            "strings are not numbers"
        );
        assert!(!matches(adult, doc! {}), "missing never satisfies a range");
        assert!(matches(doc! { "n": { "$gt": "b" } }, doc! { "n": "c" }));
    }

    #[test]
    fn dotted_paths_and_embedded_documents() {
        assert!(matches(doc! { "a.b": 2 }, doc! { "a": { "b": 2 } }));
        assert!(
            !matches(doc! { "a.b": 2 }, doc! { "a": [ { "b": 2 } ] }),
            "arrays are not traversed yet"
        );
        assert!(
            matches(doc! { "a": { "b": 2 } }, doc! { "a": { "b": 2 } }),
            "a non-operator document is an equality match"
        );
    }

    #[test]
    fn and_or() {
        let f = doc! { "$or": [ { "a": 1 }, { "b": { "$gt": 5 } } ] };
        assert!(matches(f.clone(), doc! { "a": 1 }));
        assert!(matches(f.clone(), doc! { "b": 6 }));
        assert!(!matches(f, doc! { "a": 2, "b": 5 }));
        assert!(matches(
            doc! { "$and": [ { "a": 1 }, { "b": 2 } ] },
            doc! { "a": 1, "b": 2 }
        ));
    }

    #[test]
    fn invalid_filters() {
        for bad in [
            doc! { "$nor": [] },
            doc! { "$or": [] },
            doc! { "$or": 1 },
            doc! { "a": { "$in": [1] } },
            doc! { "a..b": 1 },
        ] {
            assert!(
                matches!(Filter::parse(&bad), Err(Error::InvalidFilter(_))),
                "{bad}"
            );
        }
    }
}
