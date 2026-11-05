//! Collection paths: a Namespace name and a Collection name, written
//! `<namespace>.<collection>`, which together identify one Collection on a
//! Node.

use std::fmt;
use std::str::FromStr;

use crate::document::validate_name;
use crate::error::{Error, Result};

/// A Collection path such as `shop.orders`: the name of a Collection and of
/// the Namespace it is in. Both names are checked when the path is made, so
/// every Collection path names a Collection that could exist.
///
/// ```
/// use psdb_core::CollectionPath;
///
/// let orders: CollectionPath = "shop.orders".parse()?;
/// assert_eq!(orders, CollectionPath::new("shop", "orders")?);
/// assert_eq!((orders.namespace(), orders.collection()), ("shop", "orders"));
/// assert_eq!(orders.to_string(), "shop.orders");
/// # Ok::<(), psdb_core::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CollectionPath {
    namespace: String,
    collection: String,
}

impl CollectionPath {
    /// The path to the Collection `collection` in the Namespace
    /// `namespace`. Fails with [`Error::InvalidName`] if either name breaks
    /// the naming rules.
    pub fn new(namespace: &str, collection: &str) -> Result<Self> {
        validate_name(namespace)?;
        validate_name(collection)?;
        Ok(Self {
            namespace: namespace.to_owned(),
            collection: collection.to_owned(),
        })
    }

    /// The Namespace name.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// The Collection name.
    pub fn collection(&self) -> &str {
        &self.collection
    }
}

impl FromStr for CollectionPath {
    type Err = Error;

    /// Parse `<namespace>.<collection>`. Names never contain `.`, so the
    /// first one separates them.
    fn from_str(s: &str) -> Result<Self> {
        let (namespace, collection) = s.split_once('.').ok_or_else(|| {
            Error::InvalidName(format!(
                "{s:?} is not a collection path; write <namespace>.<collection>"
            ))
        })?;
        Self::new(namespace, collection)
    }
}

impl fmt::Display for CollectionPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.namespace, self.collection)
    }
}
