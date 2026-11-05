//! Field paths: the dotted path to a field that the Collection Schema can
//! declare and an Index can cover, such as `address.city`.

use std::borrow::Borrow;
use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};

/// A Field path such as `address.city`: field names joined by `.`, each
/// naming a field of the embedded document before it. Checked when made: no
/// segment is empty or starts with `$`, and the path is not under `_id`,
/// which is already the key of every Document and so is never declared or
/// indexed.
///
/// ```
/// use psdb_core::FieldPath;
///
/// let city: FieldPath = "address.city".parse()?;
/// assert_eq!(city.as_str(), "address.city");
/// assert!("_id".parse::<FieldPath>().is_err());
/// # Ok::<(), psdb_core::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FieldPath(String);

impl FieldPath {
    /// The path as written, such as `address.city`.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The field names, outermost first.
    pub(crate) fn segments(&self) -> Vec<String> {
        self.0.split('.').map(str::to_owned).collect()
    }
}

impl FromStr for FieldPath {
    type Err = Error;

    /// Parse a dotted path. Fails with [`Error::InvalidSchema`] if it breaks
    /// the rules above.
    fn from_str(s: &str) -> Result<Self> {
        let mut segments = s.split('.');
        if segments
            .clone()
            .any(|seg| seg.is_empty() || seg.starts_with('$'))
        {
            return Err(Error::InvalidSchema(format!("{s:?} is not a field path")));
        }
        if segments.next() == Some("_id") {
            return Err(Error::InvalidSchema(
                "_id cannot be declared or indexed; Documents are already keyed by it".into(),
            ));
        }
        Ok(Self(s.to_owned()))
    }
}

impl fmt::Display for FieldPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Lets a map keyed by Field paths be looked up with a `&str`, such as a
/// path taken from a filter.
impl Borrow<str> for FieldPath {
    fn borrow(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        assert_eq!("a.b".parse::<FieldPath>().unwrap().segments(), ["a", "b"]);
        for bad in ["", "a..b", "$a", "a.$b", "_id", "_id.x"] {
            assert!(
                matches!(bad.parse::<FieldPath>(), Err(Error::InvalidSchema(_))),
                "{bad:?}"
            );
        }
    }
}
