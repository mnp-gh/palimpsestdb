//! Field paths: the dotted path to a field that a filter compares, the
//! Collection Schema declares and an Index covers, such as `address.city`.

use std::borrow::Borrow;
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

use crate::error::{Error, Result};

/// A Field path such as `address.city`: field names joined by `.`, each
/// naming a field of the embedded document before it. Checked when made: no
/// segment is empty or starts with `$`. The segments are split once, here,
/// so reading a field never splits the path again.
///
/// Filters may name `_id` and fields under it; the Collection Schema never
/// declares them (see [`is_under_id`](Self::is_under_id)).
///
/// ```
/// use psdb_core::FieldPath;
///
/// let city: FieldPath = "address.city".parse()?;
/// assert_eq!(city.as_str(), "address.city");
/// assert!("address..city".parse::<FieldPath>().is_err());
/// # Ok::<(), psdb_core::Error>(())
/// ```
#[derive(Debug, Clone)]
pub struct FieldPath {
    path: String,
    segments: Vec<String>,
}

impl FieldPath {
    /// The path as written, such as `address.city`.
    pub fn as_str(&self) -> &str {
        &self.path
    }

    /// The field names, outermost first.
    pub(crate) fn segments(&self) -> &[String] {
        &self.segments
    }

    /// Whether this is `_id` or a field under it, which the Collection
    /// Schema never declares and so no Index covers: Documents are already
    /// keyed by `_id`.
    pub fn is_under_id(&self) -> bool {
        self.segments[0] == "_id"
    }
}

impl FromStr for FieldPath {
    type Err = Error;

    /// Parse a dotted path. Fails with [`Error::InvalidSchema`] if it breaks
    /// the rules above.
    fn from_str(s: &str) -> Result<Self> {
        let segments: Vec<String> = s.split('.').map(str::to_owned).collect();
        if segments
            .iter()
            .any(|seg| seg.is_empty() || seg.starts_with('$'))
        {
            return Err(Error::InvalidSchema(format!("{s:?} is not a field path")));
        }
        Ok(Self {
            path: s.to_owned(),
            segments,
        })
    }
}

impl fmt::Display for FieldPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.path)
    }
}

// Equality, order and hash follow the path as written, which `Borrow<str>`
// requires; the segments are derived from it.

impl PartialEq for FieldPath {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}

impl Eq for FieldPath {}

impl PartialOrd for FieldPath {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FieldPath {
    fn cmp(&self, other: &Self) -> Ordering {
        self.path.cmp(&other.path)
    }
}

impl Hash for FieldPath {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.path.hash(state);
    }
}

/// Lets a map keyed by Field paths be looked up with a `&str`.
impl Borrow<str> for FieldPath {
    fn borrow(&self) -> &str {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        assert_eq!("a.b".parse::<FieldPath>().unwrap().segments(), ["a", "b"]);
        for bad in ["", "a..b", "$a", "a.$b"] {
            assert!(
                matches!(bad.parse::<FieldPath>(), Err(Error::InvalidSchema(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn under_id() {
        for (path, under) in [
            ("_id", true),
            ("_id.x", true),
            ("x._id", false),
            ("_idx", false),
        ] {
            assert_eq!(
                path.parse::<FieldPath>().unwrap().is_under_id(),
                under,
                "{path}"
            );
        }
    }
}
