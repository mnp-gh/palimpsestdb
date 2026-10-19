//! The Catalog: which Databases and Collections exist, stored as versioned
//! entries in Database 0 so DDL is transactional (ADR 0006).
//!
//! Each entry's value is a BSON document `{ name, id }`. IDs come from a
//! counter that only increases, so an ID is never reused for a different
//! Database or Collection. Allocating an ID writes the counter, so two
//! Transactions that both create something conflict and one retries.

use bson::{Document, doc};
use psdb_storage::StorageEngine;

use crate::document::{self, validate_name};
use crate::error::{Error, Result};
use crate::keys;
use crate::transaction::Transaction;

impl<E: StorageEngine> Transaction<'_, E> {
    /// Create a Database.
    pub fn create_database(&mut self, name: &str) -> Result<()> {
        validate_name(name)?;
        let key = keys::database_entry_key(name);
        if self.read(&key)?.is_some() {
            return Err(Error::DatabaseExists(name.to_owned()));
        }
        let id = self.allocate_id()?;
        self.write(key, Some(entry(name, id)));
        Ok(())
    }

    /// Drop a Database. Its Collections become unreachable: their Catalog
    /// entries and Documents stay behind under the old IDs until garbage
    /// collection exists.
    pub fn drop_database(&mut self, name: &str) -> Result<()> {
        self.resolve_database(name)?;
        self.write(keys::database_entry_key(name), None);
        Ok(())
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
        let id = self.allocate_id()?;
        self.write(key, Some(entry(name, id)));
        Ok(())
    }

    /// Drop a Collection. Its Documents become unreachable and stay behind
    /// until garbage collection exists.
    pub fn drop_collection(&mut self, database: &str, name: &str) -> Result<()> {
        let (db_id, _) = self.resolve_collection(database, name)?;
        self.write(keys::collection_entry_key(db_id, name), None);
        Ok(())
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
    pub(crate) fn resolve_database(&mut self, name: &str) -> Result<u64> {
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
    ) -> Result<(u64, u64)> {
        let db_id = self.resolve_database(database)?;
        let key = keys::collection_entry_key(db_id, collection);
        let value = self.read(&key)?.ok_or_else(|| Error::NoSuchCollection {
            database: database.to_owned(),
            collection: collection.to_owned(),
        })?;
        self.catalog_reads.insert(key);
        Ok((db_id, entry_id(&value)?))
    }

    fn allocate_id(&mut self) -> Result<u64> {
        let key = keys::counter_key();
        let next = match self.read(&key)? {
            Some(bytes) => read_u64(&document::decode(&bytes)?, "next")?,
            // ID 0 is the Catalog's own Database.
            None => 1,
        };
        self.write(key, Some(encode(&doc! { "next": as_i64(next + 1) })));
        Ok(next)
    }
}

fn entry(name: &str, id: u64) -> Vec<u8> {
    encode(&doc! { "name": name, "id": as_i64(id) })
}

fn entry_id(bytes: &[u8]) -> Result<u64> {
    read_u64(&document::decode(bytes)?, "id")
}

fn entry_name(bytes: &[u8]) -> Result<String> {
    document::decode(bytes)?
        .get_str("name")
        .map(str::to_owned)
        .map_err(|e| Error::Corruption(format!("catalog entry name: {e}")))
}

fn read_u64(doc: &Document, field: &str) -> Result<u64> {
    doc.get_i64(field)
        .ok()
        .and_then(|n| u64::try_from(n).ok())
        .ok_or_else(|| {
            Error::Corruption(format!(
                "catalog field {field:?} is not a non-negative int64"
            ))
        })
}

fn as_i64(n: u64) -> i64 {
    i64::try_from(n).expect("catalog IDs fit in i64")
}

fn encode(doc: &Document) -> Vec<u8> {
    doc.to_vec().expect("catalog entries always encode")
}
