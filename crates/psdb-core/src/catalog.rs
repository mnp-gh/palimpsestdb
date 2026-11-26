//! The Catalog: which Namespaces and Collections exist, stored as versioned
//! entries in Namespace 0 so DDL is transactional (ADR 0006).
//!
//! Each entry's value is a BSON document `{ name, id }`; a Collection entry
//! adds its Collection Schema and Index ID counter (ADR 0011):
//!
//! ```text
//! { name, id,
//!   fields:  [ { path, type, strict } ],
//!   nextIndexId }
//! ```
//!
//! Both are optional, so a Collection without them reads as having no
//! schema and next Index ID 0. Each Index has its own Catalog entry,
//! `{ path, unique, state }`, keyed by its Collection and Index IDs
//! (ADR 0014); its `state` is `"building"` or `"ready"` (ADR 0013).
//! Changing the schema rewrites the Collection entry, and changing an Index
//! writes its own entry; a concurrent writer to the Collection read both, so
//! it aborts with Collection Changed.
//!
//! Namespace and Collection IDs are random, and creating one also writes its
//! ID key, which dropping leaves in place, so an ID is never reused, even by
//! a Namespace or Collection that holds no Documents. Creating different
//! names concurrently never conflicts; creating the same name, or drawing
//! the same ID, does, because both Transactions write the same key.
//!
//! This module owns the entries and the Namespace and Collection changes.
//! Schema and Index changes write Catalog entries too, but live with
//! their rules in [`crate::schema`] and [`crate::index`].
//!
//! Index IDs are counted per Collection by `nextIndexId`, which only grows,
//! so they are never reused either. Creating an Index rewrites the
//! Collection entry to move the counter, so concurrent creates conflict.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;
use std::sync::RwLock;

use bson::{Bson, Document, doc};
use psdb_storage::{KeyRange, StorageEngine};

use crate::collection_path::CollectionPath;
use crate::document::{self, validate_name};
use crate::error::{Error, Result};
use crate::field_path::FieldPath;
use crate::index::{IndexDef, IndexState};
use crate::keys;
use crate::mvcc;
use crate::schema::{FieldSchema, FieldType, Schema};
use crate::transaction::{Transaction, WriteSet};

/// The newest Version of every Namespace, Collection and Index Catalog
/// entry, Tombstones included, with its Commit Timestamp. It is complete, so
/// a key missing from it was never written and is absent at every Snapshot.
#[derive(Default)]
pub(crate) struct CatalogCache(RwLock<BTreeMap<Vec<u8>, CachedVersion>>);

/// A Commit Timestamp and the value written then (`None` is a Tombstone).
type CachedVersion = (u64, Option<Vec<u8>>);

impl CatalogCache {
    /// The newest Version of every Catalog entry in `engine`, Tombstones
    /// included.
    pub(crate) fn load<E: StorageEngine>(engine: &E) -> Result<Self> {
        let mut cached = BTreeMap::new();
        let mut failure = None;
        let system = KeyRange::prefix(&keys::SYSTEM_NAMESPACE.to_be_bytes());
        engine.scan(&system, &mut |key, value| {
            // The check reads only the first 8 bytes, so it works on physical keys.
            if !keys::is_catalog_entry_key(key) {
                return ControlFlow::Continue(());
            }
            let version = keys::split_physical(key)
                .and_then(|(logical, ts)| Ok((logical.to_vec(), (ts, mvcc::decode_value(value)?))));
            match version {
                // Versions are newest first: keep the first of each key.
                Ok((logical, version)) => {
                    cached.entry(logical).or_insert(version);
                    ControlFlow::Continue(())
                }
                Err(e) => {
                    failure = Some(e);
                    ControlFlow::Break(())
                }
            }
        })?;
        match failure {
            Some(e) => Err(e),
            None => Ok(Self(RwLock::new(cached))),
        }
    }

    /// The value of the Catalog entry `logical` at `snapshot_ts`, or `None`
    /// if its newest Version is newer than the Snapshot, so only the engine
    /// has the answer.
    #[expect(clippy::option_option, reason = "the outer None means ask the engine")]
    pub(crate) fn read_at(&self, logical: &[u8], snapshot_ts: u64) -> Option<Option<Vec<u8>>> {
        match self.0.read().expect("catalog cache poisoned").get(logical) {
            None => Some(None),
            Some((ts, value)) if *ts <= snapshot_ts => Some(value.clone()),
            Some(_) => None,
        }
    }

    /// The live Catalog entries under `prefix` at `snapshot_ts`, or `None`
    /// if any of them has a Version newer than the Snapshot, so only the
    /// engine has the answer.
    pub(crate) fn scan_at(
        &self,
        prefix: &[u8],
        snapshot_ts: u64,
    ) -> Option<BTreeMap<Vec<u8>, Vec<u8>>> {
        let cached = self.0.read().expect("catalog cache poisoned");
        let mut live = BTreeMap::new();
        for (key, (ts, value)) in cached
            .range(prefix.to_vec()..)
            .take_while(|(key, _)| key.starts_with(prefix))
        {
            if *ts > snapshot_ts {
                return None;
            }
            if let Some(value) = value {
                live.insert(key.clone(), value.clone());
            }
        }
        Some(live)
    }

    /// Record the Catalog entries in a Write Set committed at `commit_ts`.
    pub(crate) fn record(&self, commit_ts: u64, write_set: &WriteSet) {
        // Copied before taking the write lock, so readers wait only for the inserts.
        let entries: Vec<_> = write_set
            .iter()
            .filter(|(key, _)| keys::is_catalog_entry_key(key))
            .map(|(key, value)| (key.clone(), (commit_ts, value.clone())))
            .collect();
        // Most commits write no Catalog entry; they skip the write lock.
        if entries.is_empty() {
            return;
        }
        self.0
            .write()
            .expect("catalog cache poisoned")
            .extend(entries);
    }

    /// Forget every Version of `keys`, which Garbage Collection deleted
    /// from the engine.
    pub(crate) fn remove(&self, keys: &BTreeSet<Vec<u8>>) {
        if keys.is_empty() {
            return;
        }
        let mut cached = self.0.write().expect("catalog cache poisoned");
        for key in keys {
            cached.remove(key);
        }
    }

    /// Forget the Tombstones committed at or before `horizon`: every Snapshot
    /// from the Horizon on reads their keys as absent either way. A key
    /// written again since has a newer timestamp, so it stays.
    pub(crate) fn purge_tombstones(&self, horizon: u64) {
        self.0
            .write()
            .expect("catalog cache poisoned")
            .retain(|_, (ts, value)| value.is_some() || *ts > horizon);
    }
}

/// A Collection's Catalog entry.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CollectionEntry {
    /// The ID of the Namespace the Collection is in. Not stored in the
    /// entry: its key holds it.
    pub(crate) namespace_id: u32,
    pub(crate) name: String,
    pub(crate) id: u32,
    pub(crate) schema: Schema,
    /// Read from the Indexes' own Catalog entries, not stored in this one.
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
        let mut schema = Schema::new();
        for field in list("fields")? {
            let path = Self::read_path(&field).ok_or_else(|| corrupt("field path"))?;
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
        Ok(Self {
            namespace_id,
            name: doc.get_str("name").map_err(|_| corrupt("name"))?.to_owned(),
            id: read_u32(&doc, "id")?,
            schema,
            indexes: Vec::new(),
            next_index_id: match doc.get("nextIndexId") {
                None => 0,
                Some(_) => read_u32(&doc, "nextIndexId")?,
            },
        })
    }

    /// The `path` field of a schema field or an Index. Stored paths were
    /// checked when declared, so `None` means the entry is corrupt.
    fn read_path(doc: &Document) -> Option<FieldPath> {
        doc.get_str("path").ok().and_then(|p| p.parse().ok())
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
        encode(&doc! {
            "name": &self.name,
            "id": i64::from(self.id),
            "fields": fields,
            "nextIndexId": i64::from(self.next_index_id),
        })
    }

    /// Decode the Index Catalog entry at `key`, whose last four bytes are the
    /// Index ID.
    fn decode_index(key: &[u8], bytes: &[u8]) -> Result<IndexDef> {
        let doc = document::decode(bytes)?;
        let corrupt = |what: &str| Error::Corruption(format!("index catalog entry {what}"));
        let id = key
            .last_chunk::<4>()
            .map(|id| u32::from_be_bytes(*id))
            .ok_or_else(|| corrupt("key"))?;
        Ok(IndexDef {
            path: Self::read_path(&doc).ok_or_else(|| corrupt("path"))?,
            id,
            unique: doc.get_bool("unique").map_err(|_| corrupt("unique"))?,
            state: match doc.get_str("state") {
                Ok("building") => IndexState::Building,
                Ok("ready") => IndexState::Ready,
                _ => return Err(corrupt("state")),
            },
        })
    }

    fn encode_index(index: &IndexDef) -> Vec<u8> {
        encode(&doc! {
            "path": index.path.as_str(),
            "unique": index.unique,
            "state": match index.state {
                IndexState::Building => "building",
                IndexState::Ready => "ready",
            },
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
    /// entries and Documents stay behind under the old IDs until Garbage
    /// Collection reclaims them.
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
    /// until Garbage Collection reclaims them.
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
    /// Transaction sees it. Records the dependencies on the Namespace, the
    /// Collection and every Index, so a writer aborts if any of them changes
    /// (ADR 0014).
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
        let mut entry = CollectionEntry::decode(ns_id, &value)?;
        for (key, bytes) in self.scan(&keys::index_catalog_prefix(ns_id, entry.id))? {
            let index = CollectionEntry::decode_index(&key, &bytes)?;
            // Writes look up each indexed field's type in the schema.
            if !entry.schema.contains_key(&index.path) {
                return Err(Error::Corruption("index on an undeclared field".into()));
            }
            entry.indexes.push(index);
            self.depend_on_catalog_entry(key);
        }
        Ok(entry)
    }

    /// Rewrite the Collection's entry: its schema and `nextIndexId`.
    pub(crate) fn write_collection_entry(&mut self, entry: &CollectionEntry) -> Result<()> {
        self.write(
            keys::collection_entry_key(entry.namespace_id, &entry.name),
            Some(entry.encode()),
        )
    }

    /// Write `index`'s Catalog entry in the Collection `entry`.
    pub(crate) fn write_index_entry(
        &mut self,
        entry: &CollectionEntry,
        index: &IndexDef,
    ) -> Result<()> {
        self.write(
            keys::index_catalog_key(entry.namespace_id, entry.id, index.id),
            Some(CollectionEntry::encode_index(index)),
        )
    }

    /// Write a Tombstone on `index`'s Catalog entry in the Collection `entry`.
    pub(crate) fn drop_index_entry(
        &mut self,
        entry: &CollectionEntry,
        index: &IndexDef,
    ) -> Result<()> {
        self.write(
            keys::index_catalog_key(entry.namespace_id, entry.id, index.id),
            None,
        )
    }

    /// A random ID for a new Namespace (`namespace_id: None`) or for a new
    /// Collection in the Namespace `namespace_id`. Indexes are numbered per
    /// Collection instead.
    fn allocate_id(&mut self, namespace_id: Option<u32>) -> Result<u32> {
        let node = self.node;
        self.allocate_id_from(
            std::iter::repeat_with(|| (node.id_generator.catalog_id)()),
            namespace_id,
        )
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

fn entry(name: &str, id: u32) -> Vec<u8> {
    encode(&doc! { "name": name, "id": i64::from(id) })
}

/// The ID in a Namespace or Collection entry.
pub(crate) fn entry_id(bytes: &[u8]) -> Result<u32> {
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
    fn catalog_cache_answers_misses_and_defers_newer_entries_to_the_engine() {
        let cache = CatalogCache::default();
        let shop = keys::namespace_entry_key("shop");
        assert_eq!(cache.read_at(&shop, 0), Some(None), "never written: absent");

        let write_set = WriteSet::from([
            (shop.clone(), Some(entry("shop", 7))),
            (keys::namespace_id_key(7), Some(Vec::new())),
        ]);
        cache.record(5, &write_set);
        assert_eq!(cache.read_at(&shop, 4), None, "newer than the Snapshot");
        assert_eq!(cache.read_at(&shop, 5), Some(Some(entry("shop", 7))));
        assert!(!keys::is_catalog_entry_key(&keys::namespace_id_key(7)));
        assert!(keys::is_catalog_entry_key(&keys::collection_entry_key(
            7, "orders"
        )));
    }

    #[test]
    fn catalog_cache_lists_a_collections_indexes_or_defers_to_the_engine() {
        let cache = CatalogCache::default();
        let prefix = keys::index_catalog_prefix(7, 9);
        let (ix0, ix1) = (
            keys::index_catalog_key(7, 9, 0),
            keys::index_catalog_key(7, 9, 1),
        );
        cache.record(3, &WriteSet::from([(ix0.clone(), Some(vec![1]))]));
        cache.record(5, &WriteSet::from([(ix1.clone(), Some(vec![2]))]));
        cache.record(6, &WriteSet::from([(ix0, None)]));
        let other = keys::index_catalog_key(7, 10, 0);
        cache.record(8, &WriteSet::from([(other, Some(vec![3]))]));

        assert_eq!(cache.scan_at(&prefix, 5), None, "Index 0 dropped since");
        assert_eq!(
            cache.scan_at(&prefix, 6),
            Some(BTreeMap::from([(ix1, vec![2])])),
            "Tombstones are left out; other Collections are not listed"
        );
    }

    #[test]
    fn purging_forgets_only_tombstones_at_or_below_the_horizon() {
        let cache = CatalogCache::default();
        let shop = keys::namespace_entry_key("shop");
        let toys = keys::namespace_entry_key("toys");
        cache.record(3, &WriteSet::from([(toys.clone(), Some(entry("toys", 8)))]));
        cache.record(4, &WriteSet::from([(shop.clone(), None)]));
        cache.record(6, &WriteSet::from([(toys.clone(), None)]));

        cache.purge_tombstones(5);
        let cached = cache.0.read().unwrap();
        assert!(!cached.contains_key(&shop), "dropped before the Horizon");
        assert_eq!(cached.get(&toys), Some(&(6, None)), "dropped after it");
    }

    #[test]
    fn reopen_loads_the_cache_from_the_engine_tombstones_included() {
        let disk = crate::wal::SimDisk::new();
        let open = || {
            Node::open_with(
                MemoryEngine::new(),
                disk.file(),
                crate::Limits::default(),
                crate::IdGenerator::default(),
            )
            .unwrap()
        };
        let node = open();
        node.create_namespace("shop").unwrap();
        let orders: CollectionPath = "shop.orders".parse().unwrap();
        node.create_collection(&orders).unwrap();
        let namespace_id = node.begin_transaction().resolve_namespace("shop").unwrap();
        node.drop_collection(&orders).unwrap();
        let dropped_at = node.latest_visible_commit_ts();
        drop(node);

        let node = open();
        let entry = keys::collection_entry_key(namespace_id, "orders");
        let cached = node.catalog_cache.0.read().unwrap();
        assert_eq!(cached.get(&entry), Some(&(dropped_at, None)));
        assert!(cached.contains_key(&keys::namespace_entry_key("shop")));
    }

    #[test]
    fn id_of_an_empty_namespace_is_in_use() {
        let node = Node::new(MemoryEngine::new()).unwrap();
        node.create_namespace("shop").unwrap();
        let mut txn = node.begin_transaction();
        let taken = txn.resolve_namespace("shop").unwrap();
        assert_eq!(txn.allocate_id_from([taken, 7], None).unwrap(), 7);
    }

    #[test]
    fn id_of_an_empty_collection_is_in_use() {
        let node = Node::new(MemoryEngine::new()).unwrap();
        node.create_namespace("shop").unwrap();
        let orders = "shop.orders".parse().unwrap();
        node.create_collection(&orders).unwrap();
        let mut txn = node.begin_transaction();
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
            let entry = node.begin_transaction().collection_entry(&orders).unwrap();
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
        let (mut t1, mut t2) = (node.begin_transaction(), node.begin_transaction());
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
