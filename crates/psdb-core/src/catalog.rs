//! The Catalog: which Namespaces and Collections exist, stored as versioned
//! entries in Namespace 0 so DDL is transactional (ADR 0006).
//!
//! Each entry's value is a BSON document `{ name, id }`. IDs are random, and
//! creating one also writes its ID key, which dropping leaves in place, so an
//! ID is never reused, even by a Namespace or Collection that holds no
//! Documents. Creating different names concurrently never conflicts;
//! creating the same name, or drawing the same ID, does, because both
//! Transactions write the same key.

use bson::{Document, doc};
use psdb_storage::StorageEngine;

use crate::document::{self, validate_name};
use crate::error::{Error, Result};
use crate::keys;
use crate::mvcc;
use crate::transaction::Transaction;

impl<E: StorageEngine> Transaction<'_, E> {
    /// Create a Namespace.
    pub fn create_namespace(&mut self, name: &str) -> Result<()> {
        validate_name(name)?;
        let key = keys::namespace_entry_key(name);
        if self.read(&key)?.is_some() {
            return Err(Error::NamespaceExists(name.to_owned()));
        }
        let id = self.allocate_id(None)?;
        self.write(key, Some(entry(name, id)));
        Ok(())
    }

    /// Drop a Namespace. Its Collections become unreachable: their Catalog
    /// entries and Documents stay behind under the old IDs until garbage
    /// collection exists.
    pub fn drop_namespace(&mut self, name: &str) -> Result<()> {
        self.resolve_namespace(name)?;
        self.write(keys::namespace_entry_key(name), None);
        Ok(())
    }

    /// The names of every Namespace, in byte order.
    pub fn list_namespaces(&mut self) -> Result<Vec<String>> {
        self.scan(&keys::namespaces_prefix())?
            .values()
            .map(|v| entry_name(v))
            .collect()
    }

    /// Create a Collection in an existing Namespace.
    pub fn create_collection(&mut self, namespace: &str, name: &str) -> Result<()> {
        validate_name(name)?;
        let ns_id = self.resolve_namespace(namespace)?;
        let key = keys::collection_entry_key(ns_id, name);
        if self.read(&key)?.is_some() {
            return Err(Error::CollectionExists {
                namespace: namespace.to_owned(),
                collection: name.to_owned(),
            });
        }
        let id = self.allocate_id(Some(ns_id))?;
        self.write(key, Some(entry(name, id)));
        Ok(())
    }

    /// Drop a Collection. Its Documents become unreachable and stay behind
    /// until garbage collection exists.
    pub fn drop_collection(&mut self, namespace: &str, name: &str) -> Result<()> {
        let (ns_id, _) = self.resolve_collection(namespace, name)?;
        self.write(keys::collection_entry_key(ns_id, name), None);
        Ok(())
    }

    /// The names of every Collection in a Namespace, in byte order.
    pub fn list_collections(&mut self, namespace: &str) -> Result<Vec<String>> {
        let ns_id = self.resolve_namespace(namespace)?;
        self.scan(&keys::collections_prefix(ns_id))?
            .values()
            .map(|v| entry_name(v))
            .collect()
    }

    /// The ID of a Namespace at this Snapshot. Records the dependency, so the
    /// commit aborts if the Namespace is dropped concurrently.
    pub(crate) fn resolve_namespace(&mut self, name: &str) -> Result<u32> {
        let key = keys::namespace_entry_key(name);
        let value = self
            .read(&key)?
            .ok_or_else(|| Error::NoSuchNamespace(name.to_owned()))?;
        self.catalog_reads.insert(key);
        entry_id(&value)
    }

    /// The Namespace and Collection IDs at this Snapshot. Records both
    /// dependencies.
    pub(crate) fn resolve_collection(
        &mut self,
        namespace: &str,
        collection: &str,
    ) -> Result<(u32, u32)> {
        let ns_id = self.resolve_namespace(namespace)?;
        let key = keys::collection_entry_key(ns_id, collection);
        let value = self.read(&key)?.ok_or_else(|| Error::NoSuchCollection {
            namespace: namespace.to_owned(),
            collection: collection.to_owned(),
        })?;
        self.catalog_reads.insert(key);
        Ok((ns_id, entry_id(&value)?))
    }

    /// A random ID for a new Namespace (`namespace_id: None`) or for a new
    /// Collection in the Namespace `namespace_id`.
    fn allocate_id(&mut self, namespace_id: Option<u32>) -> Result<u32> {
        self.allocate_id_from(std::iter::repeat_with(random_id), namespace_id)
    }

    /// The first of `candidates` whose ID key was never written, at any
    /// timestamp. Writes the ID key, so a concurrent Transaction that draws
    /// the same ID fails to commit with a Write Conflict.
    fn allocate_id_from(
        &mut self,
        candidates: impl IntoIterator<Item = u32>,
        namespace_id: Option<u32>,
    ) -> Result<u32> {
        let id_key = |id| match namespace_id {
            None => keys::namespace_id_key(id),
            Some(ns_id) => keys::collection_id_key(ns_id, id),
        };
        let id = pick_unused(candidates, |id| {
            mvcc::any_version_under(&self.node.engine, &id_key(id))
        })?;
        self.write(id_key(id), Some(Vec::new()));
        Ok(id)
    }
}

/// The first candidate that is not 0 (the Catalog's own Namespace) and not
/// in use.
fn pick_unused(
    candidates: impl IntoIterator<Item = u32>,
    in_use: impl Fn(u32) -> Result<bool>,
) -> Result<u32> {
    const ATTEMPTS: usize = 16;
    for id in candidates.into_iter().take(ATTEMPTS) {
        if id != keys::SYSTEM_NAMESPACE && !in_use(id)? {
            return Ok(id);
        }
    }
    Err(Error::Corruption(format!(
        "no unused catalog ID in {ATTEMPTS} random draws"
    )))
}

/// A random 32-bit ID.
fn random_id() -> u32 {
    getrandom::u32().expect("the OS random number generator failed")
}

fn entry(name: &str, id: u32) -> Vec<u8> {
    encode(&doc! { "name": name, "id": i64::from(id) })
}

fn entry_id(bytes: &[u8]) -> Result<u32> {
    read_u32(&document::decode(bytes)?, "id")
}

fn entry_name(bytes: &[u8]) -> Result<String> {
    document::decode(bytes)?
        .get_str("name")
        .map(str::to_owned)
        .map_err(|e| Error::Corruption(format!("catalog entry name: {e}")))
}

fn read_u32(doc: &Document, field: &str) -> Result<u32> {
    doc.get_i64(field)
        .ok()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| {
            Error::Corruption(format!(
                "catalog field {field:?} is not an int64 in the u32 range"
            ))
        })
}

fn encode(doc: &Document) -> Vec<u8> {
    doc.to_vec().expect("catalog entries always encode")
}

#[cfg(test)]
mod tests {
    use psdb_storage_memory::MemoryEngine;

    use super::*;
    use crate::Node;

    #[test]
    fn id_of_an_empty_namespace_is_in_use() {
        let node = Node::new(MemoryEngine::new());
        node.create_namespace("shop").unwrap();
        let mut txn = node.begin();
        let taken = txn.resolve_namespace("shop").unwrap();
        assert_eq!(txn.allocate_id_from([taken, 7], None).unwrap(), 7);
    }

    #[test]
    fn id_of_an_empty_collection_is_in_use() {
        let node = Node::new(MemoryEngine::new());
        node.create_namespace("shop").unwrap();
        node.create_collection("shop", "orders").unwrap();
        let mut txn = node.begin();
        let (ns_id, taken) = txn.resolve_collection("shop", "orders").unwrap();
        assert_eq!(txn.allocate_id_from([taken, 7], Some(ns_id)).unwrap(), 7);
    }

    #[test]
    fn concurrent_draws_of_one_id_conflict() {
        let node = Node::new(MemoryEngine::new());
        let (mut t1, mut t2) = (node.begin(), node.begin());
        t1.allocate_id_from([7], None).unwrap();
        t2.allocate_id_from([7], None).unwrap();
        t1.commit().unwrap();
        assert!(matches!(t2.commit(), Err(Error::WriteConflict)));
    }

    #[test]
    fn pick_unused_skips_zero_and_ids_in_use() {
        let in_use = |id| Ok(id == 5);
        assert_eq!(pick_unused([0, 5, 5, 7], in_use).unwrap(), 7);
    }

    #[test]
    fn pick_unused_gives_up_eventually() {
        assert!(pick_unused(std::iter::repeat(5), |_| Ok(true)).is_err());
    }
}
