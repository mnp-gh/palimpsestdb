//! The entry point: a [`Database`] over one Storage Engine.

use std::sync::Mutex;

use bson::{Bson, Document};
use psdb_storage::StorageEngine;

use crate::document::Limits;
use crate::error::Result;
use crate::transaction::Transaction;

/// An embedded PalimpsestDB instance over the Storage Engine `E`.
///
/// Every Database (namespace) and Collection lives in the one engine
/// (ADR 0005). v0.1 keeps nothing durable and starts empty: open it over a
/// fresh engine.
///
/// ```
/// use bson::doc;
/// use psdb_core::Database;
/// use psdb_storage_memory::MemoryEngine;
///
/// let db = Database::new(MemoryEngine::new());
/// db.create_database("shop")?;
/// db.create_collection("shop", "orders")?;
/// let id = db.insert("shop", "orders", doc! { "item": "book" })?;
/// assert_eq!(db.get("shop", "orders", &id)?.unwrap().get_str("item")?, "book");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct Database<E: StorageEngine> {
    pub(crate) engine: E,
    pub(crate) limits: Limits,
    /// The newest Commit Timestamp; new Snapshots are taken here. Holding
    /// the lock serialises commits, which keeps them applied in order.
    pub(crate) last_committed: Mutex<u64>,
}

impl<E: StorageEngine> Database<E> {
    /// Open over an empty engine with default [`Limits`].
    pub fn new(engine: E) -> Self {
        Self::with_limits(engine, Limits::default())
    }

    /// Open over an empty engine with custom [`Limits`].
    pub fn with_limits(engine: E, limits: Limits) -> Self {
        Self {
            engine,
            limits,
            last_committed: Mutex::new(0),
        }
    }

    /// The limits Documents are checked against.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Begin a Transaction with a Snapshot of everything committed so far.
    pub fn begin(&self) -> Transaction<'_, E> {
        let snapshot_ts = *self.last_committed.lock().expect("lock poisoned");
        Transaction::new(self, snapshot_ts)
    }

    /// Run `work` in a new Transaction and commit it if `work` succeeds.
    /// If `work` fails, the Transaction is aborted and its error returned.
    pub fn run<T>(&self, work: impl FnOnce(&mut Transaction<'_, E>) -> Result<T>) -> Result<T> {
        let mut txn = self.begin();
        let out = work(&mut txn)?;
        txn.commit()?;
        Ok(out)
    }

    /// Autocommit [`Transaction::create_database`].
    pub fn create_database(&self, name: &str) -> Result<()> {
        self.run(|t| t.create_database(name))
    }

    /// Autocommit [`Transaction::drop_database`].
    pub fn drop_database(&self, name: &str) -> Result<()> {
        self.run(|t| t.drop_database(name))
    }

    /// Autocommit [`Transaction::list_databases`].
    pub fn list_databases(&self) -> Result<Vec<String>> {
        self.run(|t| t.list_databases())
    }

    /// Autocommit [`Transaction::create_collection`].
    pub fn create_collection(&self, database: &str, name: &str) -> Result<()> {
        self.run(|t| t.create_collection(database, name))
    }

    /// Autocommit [`Transaction::drop_collection`].
    pub fn drop_collection(&self, database: &str, name: &str) -> Result<()> {
        self.run(|t: &mut Transaction<'_, E>| t.drop_collection(database, name))
    }

    /// Autocommit [`Transaction::list_collections`].
    pub fn list_collections(&self, database: &str) -> Result<Vec<String>> {
        self.run(|t| t.list_collections(database))
    }

    /// Autocommit [`Transaction::insert`].
    pub fn insert(&self, database: &str, collection: &str, doc: Document) -> Result<Bson> {
        self.run(|t| t.insert(database, collection, doc))
    }

    /// Autocommit [`Transaction::get`].
    pub fn get(&self, database: &str, collection: &str, id: &Bson) -> Result<Option<Document>> {
        self.run(|t| t.get(database, collection, id))
    }

    /// Autocommit [`Transaction::replace`].
    pub fn replace(
        &self,
        database: &str,
        collection: &str,
        id: &Bson,
        doc: Document,
    ) -> Result<bool> {
        self.run(|t| t.replace(database, collection, id, doc))
    }

    /// Autocommit [`Transaction::delete`].
    pub fn delete(&self, database: &str, collection: &str, id: &Bson) -> Result<bool> {
        self.run(|t| t.delete(database, collection, id))
    }

    /// Autocommit [`Transaction::find`].
    pub fn find(
        &self,
        database: &str,
        collection: &str,
        filter: &Document,
    ) -> Result<Vec<Document>> {
        self.run(|t| t.find(database, collection, filter))
    }
}
