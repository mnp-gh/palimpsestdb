//! The Catalog: which Namespaces and Collections exist, stored as versioned
//! entries in Namespace 0 so DDL is transactional (ADR 0006).
//!
//! Each entry's value is a BSON document `{ name, id }`; a Collection entry
//! adds its Collection Schema and Indexes (ADR 0011):
//!
//! ```text
//! { name, id,
//!   fields:  [ { path, type, strict } ],
//!   indexes: [ { path, id, unique } ],
//!   nextIndexId }
//! ```
//!
//! All three are optional, so a Collection without them reads as having no
//! schema, no Indexes and next Index ID 0. Changing the schema or the
//! Indexes rewrites the entry, so a concurrent writer to the Collection
//! aborts with Collection Changed.
//!
//! Namespace and Collection IDs are random, and creating one also writes its
//! ID key, which dropping leaves in place, so an ID is never reused, even by
//! a Namespace or Collection that holds no Documents. Creating different
//! names concurrently never conflicts; creating the same name, or drawing
//! the same ID, does, because both Transactions write the same key.
//!
//! This module owns the entries and the Namespace and Collection changes.
//! Schema and Index changes rewrite a Collection's entry too, but live with
//! their rules in [`crate::schema`] and [`crate::index`].
//!
//! Index IDs are counted per Collection by `nextIndexId`, which only grows,
//! so they are never reused either. Creating an Index rewrites its
//! Collection's entry anyway, so the counter adds no conflicts.

use bson::{Bson, Document, doc};
use psdb_storage::{KeyRange, StorageEngine};

use crate::collection_path::CollectionPath;
use crate::document::{self, validate_name};
use crate::error::{Error, Result};
use crate::field_path::FieldPath;
use crate::index::IndexDef;
use crate::keys;
use crate::mvcc;
use crate::schema::{FieldSchema, FieldType, Schema};
use crate::transaction::Transaction;

/// A Collection's Catalog entry.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CollectionEntry {
    /// The ID of the Namespace the Collection is in. Not stored in the
    /// entry: its key holds it.
    pub(crate) namespace_id: u32,
    pub(crate) name: String,
    pub(crate) id: u32,
    pub(crate) schema: Schema,
    pub(crate) indexes: Vec<IndexDef>,
    /// The lowest Index ID never given out in this Collection.
    pub(crate) next_index_id: u32,
}

impl CollectionEntry {
    fn decode(namespace_id: u32, bytes: &[u8]) -> Result<Self> {
        let doc = document::decode(bytes)?;
        let corrupt = |what: &str| Error::Corruption(format!("catalog entry {what}"));
        let list = |field: &str| -> Result<Vec<Document>> {
            match doc.get(field) {
                None => Ok(Vec::new()),
                Some(Bson::Array(items)) => items
                    .iter()
                    .map(|item| item.as_document().cloned().ok_or_else(|| corrupt(field)))
                    .collect(),
                Some(_) => Err(corrupt(field)),
            }
        };
        // Stored paths were checked when declared; one that fails now is
        // corrupt.
        let path = |item: &Document, what: &str| -> Result<FieldPath> {
            item.get_str("path")
                .ok()
                .and_then(|p| p.parse().ok())
                .ok_or_else(|| corrupt(what))
        };
        let mut schema = Schema::new();
        for field in list("fields")? {
            let path = path(&field, "field path")?;
            let field_type = field
                .get_str("type")
                .ok()
                .and_then(|t| t.parse::<FieldType>().ok())
                .ok_or_else(|| corrupt("field type"))?;
            let strict = field
                .get_bool("strict")
                .map_err(|_| corrupt("field strict"))?;
            schema.insert(path, FieldSchema { field_type, strict });
        }
        let indexes = list("indexes")?
            .iter()
            .map(|ix| {
                Ok(IndexDef {
                    path: path(ix, "index path")?,
                    id: read_u32(ix, "id")?,
                    unique: ix.get_bool("unique").map_err(|_| corrupt("index unique"))?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        // Writes look up each indexed field's type in the schema.
        if indexes.iter().any(|ix| !schema.contains_key(&ix.path)) {
            return Err(corrupt("index on an undeclared field"));
        }
        Ok(Self {
            namespace_id,
            name: doc.get_str("name").map_err(|_| corrupt("name"))?.to_owned(),
            id: read_u32(&doc, "id")?,
            schema,
            indexes,
            next_index_id: match doc.get("nextIndexId") {
                None => 0,
                Some(_) => read_u32(&doc, "nextIndexId")?,
            },
        })
    }

    fn encode(&self) -> Vec<u8> {
        let fields: Vec<Bson> = self
            .schema
            .iter()
            .map(|(path, f)| {
                Bson::Document(
                    doc! { "path": path.as_str(), "type": f.field_type.name(), "strict": f.strict },
                )
            })
            .collect();
        let indexes: Vec<Bson> = self
            .indexes
            .iter()
            .map(|ix| {
                Bson::Document(
                    doc! { "path": ix.path.as_str(), "id": i64::from(ix.id), "unique": ix.unique },
                )
            })
            .collect();
        encode(&doc! {
            "name": &self.name,
            "id": i64::from(self.id),
            "fields": fields,
            "indexes": indexes,
            "nextIndexId": i64::from(self.next_index_id),
        })
    }

    /// The prefix of every key the Collection owns.
    pub(crate) fn prefix(&self) -> Vec<u8> {
        keys::collection_prefix(self.namespace_id, self.id)
    }

    /// The Collection's Documents, without its Index entries.
    pub(crate) fn documents(&self) -> KeyRange {
        keys::documents_range(self.namespace_id, self.id)
    }
}

impl<E: StorageEngine> Transaction<'_, E> {
    /// Create a Namespace.
    pub fn create_namespace(&mut self, name: &str) -> Result<()> {
        validate_name(name)?;
        let key = keys::namespace_entry_key(name);
        if self.read(&key)?.is_some() {
            return Err(Error::NamespaceExists(name.to_owned()));
        }
        let id = self.allocate_id(None)?;
        self.write(key, Some(entry(name, id)))
    }

    /// Drop a Namespace. Its Collections become unreachable: their Catalog
    /// entries and Documents stay behind under the old IDs until garbage
    /// collection exists.
    pub fn drop_namespace(&mut self, name: &str) -> Result<()> {
        self.resolve_namespace(name)?;
        self.write(keys::namespace_entry_key(name), None)
    }

    /// The names of every Namespace, in byte order.
    pub fn list_namespaces(&mut self) -> Result<Vec<String>> {
        self.scan(&keys::namespaces_prefix())?
            .values()
            .map(|v| entry_name(v))
            .collect()
    }

    /// Create a Collection in an existing Namespace.
    pub fn create_collection(&mut self, collection: &CollectionPath) -> Result<()> {
        let ns_id = self.resolve_namespace(collection.namespace())?;
        let key = keys::collection_entry_key(ns_id, collection.collection());
        if self.read(&key)?.is_some() {
            return Err(Error::CollectionExists(collection.clone()));
        }
        let id = self.allocate_id(Some(ns_id))?;
        self.write(key, Some(entry(collection.collection(), id)))
    }

    /// Drop a Collection. Its Documents become unreachable and stay behind
    /// until garbage collection exists.
    pub fn drop_collection(&mut self, collection: &CollectionPath) -> Result<()> {
        let (ns_id, _) = self.resolve_collection(collection)?;
        self.write(
            keys::collection_entry_key(ns_id, collection.collection()),
            None,
        )
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
        self.depend_on_catalog_entry(key);
        entry_id(&value)
    }

    /// The Namespace and Collection IDs at this Snapshot. Records both
    /// dependencies.
    pub(crate) fn resolve_collection(&mut self, collection: &CollectionPath) -> Result<(u32, u32)> {
        let entry = self.collection_entry(collection)?;
        Ok((entry.namespace_id, entry.id))
    }

    /// The Collection's entry, with its schema and Indexes, as this
    /// Transaction sees it. Records the dependencies on both the Namespace
    /// and the Collection.
    pub(crate) fn collection_entry(
        &mut self,
        collection: &CollectionPath,
    ) -> Result<CollectionEntry> {
        let ns_id = self.resolve_namespace(collection.namespace())?;
        let key = keys::collection_entry_key(ns_id, collection.collection());
        let value = self
            .read(&key)?
            .ok_or_else(|| Error::NoSuchCollection(collection.clone()))?;
        self.depend_on_catalog_entry(key);
        CollectionEntry::decode(ns_id, &value)
    }

    pub(crate) fn write_collection_entry(&mut self, entry: &CollectionEntry) -> Result<()> {
        self.write(
            keys::collection_entry_key(entry.namespace_id, &entry.name),
            Some(entry.encode()),
        )
    }

    /// A random ID for a new Namespace (`namespace_id: None`) or for a new
    /// Collection in the Namespace `namespace_id`. Indexes are numbered per
    /// Collection instead.
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
        self.write(id_key(id), Some(Vec::new()))?;
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
    Err(Error::IdsExhausted)
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
        let node = Node::new(MemoryEngine::new()).unwrap();
        node.create_namespace("shop").unwrap();
        let mut txn = node.begin();
        let taken = txn.resolve_namespace("shop").unwrap();
        assert_eq!(txn.allocate_id_from([taken, 7], None).unwrap(), 7);
    }

    #[test]
    fn id_of_an_empty_collection_is_in_use() {
        let node = Node::new(MemoryEngine::new()).unwrap();
        node.create_namespace("shop").unwrap();
        let orders = "shop.orders".parse().unwrap();
        node.create_collection(&orders).unwrap();
        let mut txn = node.begin();
        let (ns_id, taken) = txn.resolve_collection(&orders).unwrap();
        assert_eq!(txn.allocate_id_from([taken, 7], Some(ns_id)).unwrap(), 7);
    }

    #[test]
    fn index_ids_are_counted_per_collection_and_never_reused() {
        let node = Node::new(MemoryEngine::new()).unwrap();
        node.create_namespace("shop").unwrap();
        let orders = "shop.orders".parse().unwrap();
        node.create_collection(&orders).unwrap();
        let int = FieldSchema {
            field_type: FieldType::Int,
            strict: false,
        };
        let n = "n".parse().unwrap();
        node.set_field_type(&orders, &n, int).unwrap();
        let index_ids = || {
            let entry = node.begin().collection_entry(&orders).unwrap();
            entry.indexes.iter().map(|ix| ix.id).collect::<Vec<_>>()
        };
        node.create_index(&orders, &n, false).unwrap();
        assert_eq!(index_ids(), [0]);
        // Never held an entry, so only the counter keeps 0 from coming back.
        node.drop_index(&orders, &n).unwrap();
        node.create_index(&orders, &n, false).unwrap();
        assert_eq!(index_ids(), [1]);
    }

    #[test]
    fn concurrent_draws_of_one_id_conflict() {
        let node = Node::new(MemoryEngine::new()).unwrap();
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
        assert!(matches!(
            pick_unused(std::iter::repeat(5), |_| Ok(true)),
            Err(Error::IdsExhausted)
        ));
    }
}
