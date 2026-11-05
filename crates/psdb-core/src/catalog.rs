//! The Catalog: which Databases and Collections exist, stored as versioned
//! entries in Database 0 so DDL is transactional (ADR 0006).
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
//! Database and Collection IDs are random, and creating one also writes its
//! ID key, which dropping leaves in place, so an ID is never reused, even by
//! a Database or Collection that holds no Documents. Creating different
//! names concurrently never conflicts; creating the same name, or drawing
//! the same ID, does, because both Transactions write the same key.
//!
//! Index IDs are counted per Collection by `nextIndexId`, which only grows,
//! so they are never reused either. Creating an Index rewrites its
//! Collection's entry anyway, so the counter adds no conflicts.

use std::collections::BTreeMap;

use bson::{Bson, Document, doc};
use psdb_storage::StorageEngine;

use crate::document::{self, validate_name};
use crate::error::{Error, Result};
use crate::index::{IndexDef, IndexInfo};
use crate::keys;
use crate::mvcc;
use crate::schema::{self, FieldSchema, FieldType, Schema};
use crate::transaction::Transaction;

/// A Collection's Catalog entry.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CollectionEntry {
    pub(crate) name: String,
    pub(crate) id: u32,
    pub(crate) schema: Schema,
    pub(crate) indexes: Vec<IndexDef>,
    /// The lowest Index ID never given out in this Collection.
    next_index_id: u32,
}

impl CollectionEntry {
    fn decode(bytes: &[u8]) -> Result<Self> {
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
            let path = field.get_str("path").map_err(|_| corrupt("field path"))?;
            let field_type = field
                .get_str("type")
                .ok()
                .and_then(|t| t.parse::<FieldType>().ok())
                .ok_or_else(|| corrupt("field type"))?;
            let strict = field
                .get_bool("strict")
                .map_err(|_| corrupt("field strict"))?;
            schema.insert(path.to_owned(), FieldSchema { field_type, strict });
        }
        let indexes = list("indexes")?
            .iter()
            .map(|ix| {
                Ok(IndexDef {
                    path: ix
                        .get_str("path")
                        .map_err(|_| corrupt("index path"))?
                        .to_owned(),
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

    /// Fail with [`Error::TypeMismatch`] if `doc` breaks any Strict Field.
    pub(crate) fn check_strict(&self, doc: &bson::RawDocument) -> Result<()> {
        for (path, field) in self.schema.iter().filter(|(_, f)| f.strict) {
            schema::check_field(doc, path, &schema::segments(path), *field)?;
        }
        Ok(())
    }

    fn encode(&self) -> Vec<u8> {
        let fields: Vec<Bson> = self
            .schema
            .iter()
            .map(|(path, f)| {
                Bson::Document(
                    doc! { "path": path, "type": f.field_type.name(), "strict": f.strict },
                )
            })
            .collect();
        let indexes: Vec<Bson> = self
            .indexes
            .iter()
            .map(|ix| {
                Bson::Document(
                    doc! { "path": &ix.path, "id": i64::from(ix.id), "unique": ix.unique },
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

    fn index_on(&self, path: &str) -> Option<&IndexDef> {
        self.indexes.iter().find(|ix| ix.path == path)
    }
}

impl<E: StorageEngine> Transaction<'_, E> {
    /// Create a Database.
    pub fn create_database(&mut self, name: &str) -> Result<()> {
        validate_name(name)?;
        let key = keys::database_entry_key(name);
        if self.read(&key)?.is_some() {
            return Err(Error::DatabaseExists(name.to_owned()));
        }
        let id = self.allocate_id(None)?;
        self.write(key, Some(entry(name, id)))
    }

    /// Drop a Database. Its Collections become unreachable: their Catalog
    /// entries and Documents stay behind under the old IDs until garbage
    /// collection exists.
    pub fn drop_database(&mut self, name: &str) -> Result<()> {
        self.resolve_database(name)?;
        self.write(keys::database_entry_key(name), None)
    }

    /// The names of every Database, in byte order.
    pub fn list_databases(&mut self) -> Result<Vec<String>> {
        self.scan(&keys::databases_prefix())?
            .values()
            .map(|v| entry_name(v))
            .collect()
    }

    /// Create a Collection in an existing Database.
    pub fn create_collection(&mut self, database: &str, name: &str) -> Result<()> {
        validate_name(name)?;
        let db_id = self.resolve_database(database)?;
        let key = keys::collection_entry_key(db_id, name);
        if self.read(&key)?.is_some() {
            return Err(Error::CollectionExists {
                database: database.to_owned(),
                collection: name.to_owned(),
            });
        }
        let id = self.allocate_id(Some(db_id))?;
        self.write(key, Some(entry(name, id)))
    }

    /// Drop a Collection. Its Documents become unreachable and stay behind
    /// until garbage collection exists.
    pub fn drop_collection(&mut self, database: &str, name: &str) -> Result<()> {
        let (db_id, _) = self.resolve_collection(database, name)?;
        self.write(keys::collection_entry_key(db_id, name), None)
    }

    /// The names of every Collection in a Database, in byte order.
    pub fn list_collections(&mut self, database: &str) -> Result<Vec<String>> {
        let db_id = self.resolve_database(database)?;
        self.scan(&keys::collections_prefix(db_id))?
            .values()
            .map(|v| entry_name(v))
            .collect()
    }

    /// The ID of a Database at this Snapshot. Records the dependency, so the
    /// commit aborts if the Database is dropped concurrently.
    pub(crate) fn resolve_database(&mut self, name: &str) -> Result<u32> {
        let key = keys::database_entry_key(name);
        let value = self
            .read(&key)?
            .ok_or_else(|| Error::NoSuchDatabase(name.to_owned()))?;
        self.catalog_reads.insert(key);
        entry_id(&value)
    }

    /// The Database and Collection IDs at this Snapshot. Records both
    /// dependencies.
    pub(crate) fn resolve_collection(
        &mut self,
        database: &str,
        collection: &str,
    ) -> Result<(u32, u32)> {
        let (db_id, entry) = self.collection_entry(database, collection)?;
        Ok((db_id, entry.id))
    }

    /// The Database ID and the Collection's entry, with its schema and
    /// Indexes, as this Transaction sees them. Records both dependencies.
    pub(crate) fn collection_entry(
        &mut self,
        database: &str,
        collection: &str,
    ) -> Result<(u32, CollectionEntry)> {
        let db_id = self.resolve_database(database)?;
        let key = keys::collection_entry_key(db_id, collection);
        let value = self.read(&key)?.ok_or_else(|| Error::NoSuchCollection {
            database: database.to_owned(),
            collection: collection.to_owned(),
        })?;
        self.catalog_reads.insert(key);
        Ok((db_id, CollectionEntry::decode(&value)?))
    }

    pub(crate) fn write_collection_entry(
        &mut self,
        db_id: u32,
        entry: &CollectionEntry,
    ) -> Result<()> {
        self.write(
            keys::collection_entry_key(db_id, &entry.name),
            Some(entry.encode()),
        )
    }

    /// Declare a field's Field Type in the Collection Schema, or change its
    /// declaration.
    ///
    /// While an Index on the field exists, its type cannot change, and a
    /// field with a unique Index cannot stop being strict: drop the Index
    /// first. Making a field strict reads every Document and fails with
    /// [`Error::TypeMismatch`] if any breaks the rule.
    pub fn set_field_type(
        &mut self,
        database: &str,
        collection: &str,
        path: &str,
        field: FieldSchema,
    ) -> Result<()> {
        let segments = schema::split_path(path)?;
        let (db_id, mut entry) = self.collection_entry(database, collection)?;
        let old = entry.schema.get(path).copied();
        if let (Some(ix), Some(old)) = (entry.index_on(path), old) {
            if old.field_type != field.field_type {
                return Err(Error::InvalidSchema(format!(
                    "{path} has an index; drop it before changing the field's type"
                )));
            }
            if ix.unique && !field.strict {
                return Err(Error::InvalidSchema(format!(
                    "{path} has a unique index, which needs a strict field; drop it first"
                )));
            }
        }
        let already_enforced = old.is_some_and(|o| o.strict && o.field_type == field.field_type);
        if field.strict && !already_enforced {
            let documents = keys::documents_range(db_id, entry.id);
            for bytes in self.scan_range(&documents)?.values() {
                schema::check_field(document::raw(bytes)?, path, &segments, field)?;
            }
            // A Document committed concurrently was not checked.
            self.collection_reads
                .insert(keys::collection_prefix(db_id, entry.id));
        }
        entry.schema.insert(path.to_owned(), field);
        self.write_collection_entry(db_id, &entry)
    }

    /// Remove a field's declaration from the Collection Schema. Refused
    /// while an Index on the field exists.
    pub fn remove_field_type(
        &mut self,
        database: &str,
        collection: &str,
        path: &str,
    ) -> Result<()> {
        let (db_id, mut entry) = self.collection_entry(database, collection)?;
        if entry.index_on(path).is_some() {
            return Err(Error::InvalidSchema(format!(
                "{path} has an index; drop it before removing the field's type"
            )));
        }
        if entry.schema.remove(path).is_none() {
            return Err(Error::InvalidSchema(format!("{path} is not declared")));
        }
        self.write_collection_entry(db_id, &entry)
    }

    /// The Collection Schema: every declared field path and its type.
    pub fn collection_schema(
        &mut self,
        database: &str,
        collection: &str,
    ) -> Result<BTreeMap<String, FieldSchema>> {
        Ok(self.collection_entry(database, collection)?.1.schema)
    }

    /// Create an Index on a field of an empty Collection, as part of this
    /// Transaction (ADR 0010). There is nothing to build, so it commits or
    /// aborts with the rest of the Transaction.
    ///
    /// Fails with [`Error::CollectionNotEmpty`] if the Collection holds
    /// Documents: use [`Database::create_index`](crate::Database::create_index),
    /// which builds the Index on its own. A Document committed concurrently
    /// makes this Transaction abort with Collection Changed.
    pub fn create_index(
        &mut self,
        database: &str,
        collection: &str,
        path: &str,
        unique: bool,
    ) -> Result<()> {
        let (db_id, mut entry, index) = self.prepare_index(database, collection, path, unique)?;
        if self.any_live_in(&keys::documents_range(db_id, entry.id))? {
            return Err(Error::CollectionNotEmpty {
                database: database.to_owned(),
                collection: collection.to_owned(),
            });
        }
        self.collection_reads
            .insert(keys::collection_prefix(db_id, entry.id));
        entry.indexes.push(index);
        self.write_collection_entry(db_id, &entry)
    }

    /// Check that an Index may be created and give it the Collection's next
    /// Index ID. The caller fills the Index and adds it to the returned
    /// entry, whose counter has moved past the ID.
    pub(crate) fn prepare_index(
        &mut self,
        database: &str,
        collection: &str,
        path: &str,
        unique: bool,
    ) -> Result<(u32, CollectionEntry, IndexDef)> {
        let (db_id, mut entry) = self.collection_entry(database, collection)?;
        let Some(field) = entry.schema.get(path) else {
            return Err(Error::InvalidSchema(format!(
                "declare the type of {path} in the collection schema before indexing it"
            )));
        };
        if entry.index_on(path).is_some() {
            return Err(Error::IndexExists {
                database: database.to_owned(),
                collection: collection.to_owned(),
                path: path.to_owned(),
            });
        }
        if unique && !field.strict {
            return Err(Error::InvalidSchema(format!(
                "a unique index needs a strict field; declare {path} strict first"
            )));
        }
        // Skip IDs holding entries from a build that never committed: the
        // counter moves only when the Index is added (ADR 0010).
        let mut id = entry.next_index_id;
        while mvcc::any_version_under(&self.db.engine, &keys::index_prefix(db_id, entry.id, id))? {
            id += 1;
        }
        entry.next_index_id = id + 1;
        Ok((
            db_id,
            entry,
            IndexDef {
                path: path.to_owned(),
                id,
                unique,
            },
        ))
    }

    /// Drop the Index on a field. Its entries become unreachable and stay
    /// behind until garbage collection exists.
    pub fn drop_index(&mut self, database: &str, collection: &str, path: &str) -> Result<()> {
        let (db_id, mut entry) = self.collection_entry(database, collection)?;
        let before = entry.indexes.len();
        entry.indexes.retain(|ix| ix.path != path);
        if entry.indexes.len() == before {
            return Err(Error::NoSuchIndex {
                database: database.to_owned(),
                collection: collection.to_owned(),
                path: path.to_owned(),
            });
        }
        self.write_collection_entry(db_id, &entry)
    }

    /// The Collection's Indexes, in the order they were created.
    pub fn list_indexes(&mut self, database: &str, collection: &str) -> Result<Vec<IndexInfo>> {
        Ok(self
            .collection_entry(database, collection)?
            .1
            .indexes
            .into_iter()
            .map(|ix| IndexInfo {
                path: ix.path,
                unique: ix.unique,
            })
            .collect())
    }

    /// A random ID for a new Database (`database: None`) or for a new
    /// Collection in `database`. Indexes are numbered per Collection
    /// instead.
    fn allocate_id(&mut self, database: Option<u32>) -> Result<u32> {
        self.allocate_id_from(std::iter::repeat_with(random_id), database)
    }

    /// The first of `candidates` whose ID key was never written, at any
    /// timestamp. Writes the ID key, so a concurrent Transaction that draws
    /// the same ID fails to commit with a Write Conflict.
    fn allocate_id_from(
        &mut self,
        candidates: impl IntoIterator<Item = u32>,
        database: Option<u32>,
    ) -> Result<u32> {
        let id_key = |id| match database {
            None => keys::database_id_key(id),
            Some(db_id) => keys::collection_id_key(db_id, id),
        };
        let id = pick_unused(candidates, |id| {
            mvcc::any_version_under(&self.db.engine, &id_key(id))
        })?;
        self.write(id_key(id), Some(Vec::new()))?;
        Ok(id)
    }
}

/// The first candidate that is not 0 (the Catalog's own Database) and not
/// in use.
fn pick_unused(
    candidates: impl IntoIterator<Item = u32>,
    in_use: impl Fn(u32) -> Result<bool>,
) -> Result<u32> {
    const ATTEMPTS: usize = 16;
    for id in candidates.into_iter().take(ATTEMPTS) {
        if id != keys::SYSTEM_DATABASE && !in_use(id)? {
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
    use crate::Database;

    #[test]
    fn id_of_an_empty_database_is_in_use() {
        let db = Database::new(MemoryEngine::new());
        db.create_database("shop").unwrap();
        let mut txn = db.begin();
        let taken = txn.resolve_database("shop").unwrap();
        assert_eq!(txn.allocate_id_from([taken, 7], None).unwrap(), 7);
    }

    #[test]
    fn id_of_an_empty_collection_is_in_use() {
        let db = Database::new(MemoryEngine::new());
        db.create_database("shop").unwrap();
        db.create_collection("shop", "orders").unwrap();
        let mut txn = db.begin();
        let (db_id, taken) = txn.resolve_collection("shop", "orders").unwrap();
        assert_eq!(txn.allocate_id_from([taken, 7], Some(db_id)).unwrap(), 7);
    }

    #[test]
    fn index_ids_are_counted_per_collection_and_never_reused() {
        let db = Database::new(MemoryEngine::new());
        db.create_database("shop").unwrap();
        db.create_collection("shop", "orders").unwrap();
        let int = FieldSchema {
            field_type: FieldType::Int,
            strict: false,
        };
        db.set_field_type("shop", "orders", "n", int).unwrap();
        let index_ids = || {
            let (_, entry) = db.begin().collection_entry("shop", "orders").unwrap();
            entry.indexes.iter().map(|ix| ix.id).collect::<Vec<_>>()
        };
        db.create_index("shop", "orders", "n", false).unwrap();
        assert_eq!(index_ids(), [0]);
        // Never held an entry, so only the counter keeps 0 from coming back.
        db.drop_index("shop", "orders", "n").unwrap();
        db.create_index("shop", "orders", "n", false).unwrap();
        assert_eq!(index_ids(), [1]);
    }

    #[test]
    fn concurrent_draws_of_one_id_conflict() {
        let db = Database::new(MemoryEngine::new());
        let (mut t1, mut t2) = (db.begin(), db.begin());
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
