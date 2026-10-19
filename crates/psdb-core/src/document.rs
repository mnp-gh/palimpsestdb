//! Document and name rules: limits, Document IDs, Database and Collection
//! names.

use bson::{Bson, Document, oid::ObjectId};

use crate::error::{Error, Result};

/// The largest Database or Collection name accepted, in bytes. Fixed by the
/// key format.
pub const MAX_NAME_BYTES: usize = 64;

/// Configurable limits on Documents.
///
/// Limits fixed by the on-disk format, such as [`MAX_NAME_BYTES`] and
/// [`MAX_ID_BYTES`](crate::MAX_ID_BYTES), are constants instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// The largest Document accepted, in encoded BSON bytes.
    pub max_document_bytes: usize,
    /// The deepest nesting of documents and arrays accepted. A Document with
    /// no nested values has depth 1.
    pub max_nesting_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_document_bytes: 16 * 1024 * 1024,
            max_nesting_depth: 100,
        }
    }
}

/// Check a Database or Collection name: 1 to [`MAX_NAME_BYTES`] bytes, with
/// no `.` (it separates Database from Collection) and no NUL.
pub(crate) fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::InvalidName("names must not be empty".into()));
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(Error::InvalidName(format!(
            "{name:?} is longer than {MAX_NAME_BYTES} bytes"
        )));
    }
    if name.contains(['.', '\0']) {
        return Err(Error::InvalidName(format!("{name:?} contains '.' or NUL")));
    }
    Ok(())
}

/// Validate `doc` against `limits` and return it with its Document ID as
/// the first field. If `doc` has no `_id`, `id` is used, or a new ObjectId
/// is generated when `id` is `None`. If both are present they must be equal.
pub(crate) fn prepare(
    doc: Document,
    id: Option<&Bson>,
    limits: &Limits,
) -> Result<(Bson, Vec<u8>)> {
    let mut doc = doc;
    let id = match (doc.remove("_id"), id) {
        (Some(own), Some(given)) if own != *given => {
            return Err(Error::InvalidDocument(format!(
                "document _id {own} does not match the _id being replaced, {given}"
            )));
        }
        (Some(own), _) => own,
        (None, Some(given)) => given.clone(),
        (None, None) => Bson::ObjectId(ObjectId::new()),
    };

    let mut normalized = Document::new();
    normalized.insert("_id", id.clone());
    normalized.extend(doc);

    check_depth(&normalized, limits.max_nesting_depth)?;
    let bytes = normalized
        .to_vec()
        .map_err(|e| Error::InvalidDocument(format!("cannot encode as BSON: {e}")))?;
    if bytes.len() > limits.max_document_bytes {
        return Err(Error::InvalidDocument(format!(
            "document is {} bytes; the limit is {}",
            bytes.len(),
            limits.max_document_bytes
        )));
    }
    Ok((id, bytes))
}

/// Reject documents nested deeper than `max`. Iterative, so a very deep
/// document cannot overflow the stack.
fn check_depth(doc: &Document, max: usize) -> Result<()> {
    let mut stack: Vec<(&Bson, usize)> = doc.values().map(|v| (v, 2)).collect();
    if max < 1 {
        return Err(too_deep(max));
    }
    while let Some((value, depth)) = stack.pop() {
        let children: Box<dyn Iterator<Item = &Bson>> = match value {
            Bson::Document(d) => Box::new(d.values()),
            Bson::Array(a) => Box::new(a.iter()),
            _ => continue,
        };
        if depth > max {
            return Err(too_deep(max));
        }
        stack.extend(children.map(|child| (child, depth + 1)));
    }
    Ok(())
}

fn too_deep(max: usize) -> Error {
    Error::InvalidDocument(format!("nesting is deeper than the limit of {max}"))
}

/// Decode a stored Document.
pub(crate) fn decode(bytes: &[u8]) -> Result<Document> {
    Document::from_reader(bytes).map_err(|e| Error::Corruption(format!("stored document: {e}")))
}

#[cfg(test)]
mod tests {
    use bson::doc;

    use super::*;

    #[test]
    fn generated_id_is_an_object_id_placed_first() {
        let (id, bytes) = prepare(doc! { "a": 1 }, None, &Limits::default()).unwrap();
        assert!(matches!(id, Bson::ObjectId(_)));
        let stored = decode(&bytes).unwrap();
        assert_eq!(stored.keys().next().map(String::as_str), Some("_id"));
        assert_eq!(stored.get("_id"), Some(&id));
    }

    #[test]
    fn own_id_moves_first() {
        let (id, bytes) = prepare(doc! { "a": 1, "_id": "x" }, None, &Limits::default()).unwrap();
        assert_eq!(id, Bson::String("x".into()));
        assert_eq!(decode(&bytes).unwrap(), doc! { "_id": "x", "a": 1 });
    }

    #[test]
    fn mismatched_id_is_rejected() {
        let err = prepare(doc! { "_id": 1 }, Some(&Bson::Int32(2)), &Limits::default());
        assert!(matches!(err, Err(Error::InvalidDocument(_))));
    }

    #[test]
    fn depth_limit() {
        let limits = Limits {
            max_nesting_depth: 3,
            ..Limits::default()
        };
        assert!(
            prepare(doc! { "a": { "b": 1 } }, None, &limits).is_ok(),
            "depth 2"
        );
        assert!(
            prepare(doc! { "a": [ { "b": 1 } ] }, None, &limits).is_ok(),
            "depth 3"
        );
        let err = prepare(doc! { "a": [ { "b": [] } ] }, None, &limits);
        assert!(matches!(err, Err(Error::InvalidDocument(_))), "depth 4");
    }

    #[test]
    fn size_limit() {
        let limits = Limits {
            max_document_bytes: 64,
            ..Limits::default()
        };
        assert!(prepare(doc! { "s": "x".repeat(10) }, None, &limits).is_ok());
        let err = prepare(doc! { "s": "x".repeat(100) }, None, &limits);
        assert!(matches!(err, Err(Error::InvalidDocument(_))));
    }

    #[test]
    fn names() {
        assert!(validate_name("orders").is_ok());
        assert!(validate_name(&"n".repeat(MAX_NAME_BYTES)).is_ok());
        for bad in ["", "a.b", "a\0b", &"n".repeat(MAX_NAME_BYTES + 1)] {
            assert!(
                matches!(validate_name(bad), Err(Error::InvalidName(_))),
                "{bad:?}"
            );
        }
    }
}
