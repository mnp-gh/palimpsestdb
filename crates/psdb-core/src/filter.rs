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

use bson::{Bson, Document};

use crate::error::{Error, Result};

/// A parsed filter.
#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    /// Every sub-filter matches. An empty `And` matches everything.
    And(Vec<Filter>),
    /// At least one sub-filter matches.
    Or(Vec<Filter>),
    /// The value at `path` compares to `value` as `op` requires.
    Compare {
        /// The field path, split on `.`.
        path: Vec<String>,
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
    /// let filter = Filter::parse(&doc! { "age": { "$gte": 18 }, "address.city": "Paris" }).unwrap();
    /// assert!(filter.matches(&doc! { "age": 30, "address": { "city": "Paris" } }));
    /// assert!(!filter.matches(&doc! { "age": 12, "address": { "city": "Paris" } }));
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

    /// Whether `doc` matches this filter.
    pub fn matches(&self, doc: &Document) -> bool {
        match self {
            Filter::And(fs) => fs.iter().all(|f| f.matches(doc)),
            Filter::Or(fs) => fs.iter().any(|f| f.matches(doc)),
            Filter::Compare { path, op, value } => compare_at(lookup(doc, path), *op, value),
        }
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
    let segments: Vec<String> = path.split('.').map(str::to_owned).collect();
    if segments.iter().any(String::is_empty) {
        return Err(Error::InvalidFilter(format!(
            "empty segment in path {path:?}"
        )));
    }
    let compare = |op, value: &Bson| Filter::Compare {
        path: segments.clone(),
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

/// The value at `path`, descending through nested documents only.
fn lookup<'a>(doc: &'a Document, path: &[String]) -> Option<&'a Bson> {
    let (first, rest) = path.split_first()?;
    let mut current = doc.get(first)?;
    for segment in rest {
        match current {
            Bson::Document(d) => current = d.get(segment)?,
            _ => return None,
        }
    }
    Some(current)
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
/// int32, int64 and double. Documents and arrays only compare for equality.
/// Values of different types are incomparable, so range operators never
/// match across types.
fn compare(a: &Bson, b: &Bson) -> Option<Ordering> {
    use Bson::*;
    match (a, b) {
        (Int32(_) | Int64(_), Int32(_) | Int64(_)) => Some(as_i64(a)?.cmp(&as_i64(b)?)),
        (Int32(_) | Int64(_) | Double(_), Int32(_) | Int64(_) | Double(_)) => {
            as_f64(a)?.partial_cmp(&as_f64(b)?)
        }
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

fn as_f64(v: &Bson) -> Option<f64> {
    match v {
        Bson::Int32(n) => Some(f64::from(*n)),
        Bson::Int64(n) => Some(*n as f64),
        Bson::Double(n) => Some(*n),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use bson::doc;

    use super::*;

    fn matches(filter: Document, doc: Document) -> bool {
        Filter::parse(&filter).unwrap().matches(&doc)
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
