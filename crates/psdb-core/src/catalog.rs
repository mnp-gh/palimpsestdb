//! The Catalog: which Databases and Collections exist, stored as versioned
//! entries in Database 0 so DDL is transactional (ADR 0006).
//!
//! Each entry's value is a BSON document `{ name, id }`. IDs are random, and
//! creating one also writes its ID key, which dropping leaves in place, so an
//! ID is never reused, even by a Database or Collection that holds no
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
    /// Create a Database.
    pub fn create_database(&mut self, name: &str) -> Result<()> {
        validate_name(name)?;
        let key = keys::database_entry_key(name);
        if self.read(&key)?.is_some() {
            return Err(Error::DatabaseExists(name.to_owned()));
        }
        let id = self.allocate_id(None)?;
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
        let id = self.allocate_id(Some(db_id))?;
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
        let db_id = self.resolve_database(database)?;
        let key = keys::collection_entry_key(db_id, collection);
        let value = self.read(&key)?.ok_or_else(|| Error::NoSuchCollection {
            database: database.to_owned(),
            collection: collection.to_owned(),
        })?;
        self.catalog_reads.insert(key);
        Ok((db_id, entry_id(&value)?))
    }

    /// A random ID for a new Database (`database: None`) or for a new
    /// Collection in `database`.
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
        self.write(id_key(id), Some(Vec::new()));
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
        if id != keys::CATALOG_DATABASE && !in_use(id)? {
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
