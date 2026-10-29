//! The Catalog: which Databases and Collections exist, stored as versioned
//! entries in Database 0 so DDL is transactional (ADR 0006).
//!
//! Each entry's value is a BSON document `{ name, id }`. IDs are random and
//! checked against every key ever written under them, so an ID is never
//! reused, even while a dropped Collection's data is still in the engine.
//! Creating different names concurrently never conflicts; creating the same
//! name does, because both write the same Catalog entry.

use bson::{Document, doc};
use psdb_storage::StorageEngine;

use crate::document::{self, validate_name};
use crate::error::{Error, Result};
use crate::keys;
use crate::mvcc;
use crate::transaction::Transaction;

impl<E: StorageEngine> Transaction<'_, E> {
    /// Create a Database.
    pub fn create_database(&mut self, name: &str) -> Result<()> {
        validate_name(name)?;
        let key = keys::database_entry_key(name);
        if self.read(&key)?.is_some() {
            return Err(Error::DatabaseExists(name.to_owned()));
        }
        let id = self.allocate_id(&[])?;
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
        let id = self.allocate_id(&db_id.to_be_bytes())?;
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

    /// A random ID with no key ever written under `parent ‖ id`: pass no
    /// parent for a Database ID, and the Database ID's bytes for a
    /// Collection ID, matching the key layout (ADR 0008).
    ///
    /// Two Transactions drawing the same ID concurrently is not checked; with
    /// 63 random bits it is too unlikely to matter.
    fn allocate_id(&self, parent: &[u8]) -> Result<u64> {
        pick_unused(std::iter::repeat_with(random_id), |id| {
            let mut prefix = parent.to_vec();
            prefix.extend_from_slice(&id.to_be_bytes());
            mvcc::any_version_under(&self.db.engine, &prefix)
        })
    }
}

/// The first candidate that is not 0 (the Catalog's own Database) and not
/// in use.
fn pick_unused(
    candidates: impl IntoIterator<Item = u64>,
    in_use: impl Fn(u64) -> Result<bool>,
) -> Result<u64> {
    const ATTEMPTS: usize = 16;
    for id in candidates.into_iter().take(ATTEMPTS) {
        if id != keys::CATALOG_DATABASE && !in_use(id)? {
            return Ok(id);
        }
    }
    Err(Error::Corruption(format!(
        "no unused catalog ID in {ATTEMPTS} random draws"
    )))
}

/// A random ID in `0..=i64::MAX`, so it fits the BSON int64 stored in
/// Catalog entries.
fn random_id() -> u64 {
    getrandom::u64().expect("the OS random number generator failed") & (i64::MAX as u64)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_unused_skips_zero_and_ids_in_use() {
        let in_use = |id| Ok(id == 5);
        assert_eq!(pick_unused([0, 5, 5, 7], in_use).unwrap(), 7);
    }

    #[test]
    fn pick_unused_gives_up_eventually() {
        assert!(pick_unused(std::iter::repeat(5), |_| Ok(true)).is_err());
    }

    #[test]
    fn random_ids_fit_in_i64() {
        for _ in 0..1000 {
            assert!(i64::try_from(random_id()).is_ok());
        }
    }
}
