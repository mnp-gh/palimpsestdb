//! Transactions: Snapshot reads, a private Write Set, and first-committer-
//! wins Snapshot Isolation at commit.

use std::collections::{BTreeMap, BTreeSet};

use bson::{Bson, Document};
use psdb_storage::{StorageEngine, WriteBatch};

use crate::database::Database;
use crate::document;
use crate::error::{Error, Result};
use crate::filter::Filter;
use crate::keys;
use crate::mvcc;

/// A Transaction: reads see the Snapshot taken at [`Database::begin`] plus
/// this Transaction's own writes; writes stay in its Write Set until
/// [`commit`](Self::commit).
///
/// Dropping a Transaction without committing aborts it.
pub struct Transaction<'db, E: StorageEngine> {
    pub(crate) db: &'db Database<E>,
    pub(crate) snapshot_ts: u64,
    /// Logical key → new value (`None` deletes). Ordered so scans can
    /// overlay it on the Snapshot.
    pub(crate) write_set: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    /// Catalog entries this Transaction depended on; checked at commit
    /// (ADR 0006).
    pub(crate) catalog_reads: BTreeSet<Vec<u8>>,
}

impl<'db, E: StorageEngine> Transaction<'db, E> {
    pub(crate) fn new(db: &'db Database<E>, snapshot_ts: u64) -> Self {
        Self {
            db,
            snapshot_ts,
            write_set: BTreeMap::new(),
            catalog_reads: BTreeSet::new(),
        }
    }

    /// The Commit Timestamp this Transaction's Snapshot was taken at.
    pub fn snapshot_ts(&self) -> u64 {
        self.snapshot_ts
    }

    /// Read a logical key: own writes first, then the Snapshot.
    pub(crate) fn read(&self, logical: &[u8]) -> Result<Option<Vec<u8>>> {
        match self.write_set.get(logical) {
            Some(own) => Ok(own.clone()),
            None => mvcc::read_at(&self.db.engine, logical, self.snapshot_ts),
        }
    }

    pub(crate) fn write(&mut self, logical: Vec<u8>, value: Option<Vec<u8>>) {
        self.write_set.insert(logical, value);
    }

    /// Every live value under `prefix`: the Snapshot with own writes
    /// overlaid, in key order.
    pub(crate) fn scan(&self, prefix: &[u8]) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
        let mut rows = BTreeMap::new();
        mvcc::scan_at(
            &self.db.engine,
            prefix,
            self.snapshot_ts,
            &mut |key, value| {
                rows.insert(key.to_vec(), value);
            },
        )?;
        let own = self
            .write_set
            .range(prefix.to_vec()..)
            .take_while(|(k, _)| k.starts_with(prefix));
        for (key, value) in own {
            match value {
                Some(v) => rows.insert(key.clone(), v.clone()),
                None => rows.remove(key),
            };
        }
        Ok(rows)
    }

    /// Insert a Document. If it has no `_id`, an ObjectId is generated.
    /// Returns the Document ID.
    pub fn insert(&mut self, database: &str, collection: &str, doc: Document) -> Result<Bson> {
        let (db_id, coll_id) = self.resolve_collection(database, collection)?;
        let (id, bytes) = document::prepare(doc, None, &self.db.limits)?;
        let key = keys::document_key(db_id, coll_id, &id)?;
        if self.read(&key)?.is_some() {
            return Err(Error::DuplicateId(id.to_string()));
        }
        self.write(key, Some(bytes));
        Ok(id)
    }

    /// The Document with this Document ID, if it exists.
    pub fn get(&mut self, database: &str, collection: &str, id: &Bson) -> Result<Option<Document>> {
        let (db_id, coll_id) = self.resolve_collection(database, collection)?;
        let key = keys::document_key(db_id, coll_id, id)?;
        self.read(&key)?
            .map(|bytes| document::decode(&bytes))
            .transpose()
    }

    /// Replace the whole Document with this Document ID. `doc` may omit
    /// `_id`; if present it must equal `id`. Returns `false` if no such
    /// Document exists (nothing is written).
    pub fn replace(
        &mut self,
        database: &str,
        collection: &str,
        id: &Bson,
        doc: Document,
    ) -> Result<bool> {
        let (db_id, coll_id) = self.resolve_collection(database, collection)?;
        let key = keys::document_key(db_id, coll_id, id)?;
        if self.read(&key)?.is_none() {
            return Ok(false);
        }
        let (_, bytes) = document::prepare(doc, Some(id), &self.db.limits)?;
        self.write(key, Some(bytes));
        Ok(true)
    }

    /// Delete the Document with this Document ID. Returns `false` if no such
    /// Document exists.
    pub fn delete(&mut self, database: &str, collection: &str, id: &Bson) -> Result<bool> {
        let (db_id, coll_id) = self.resolve_collection(database, collection)?;
        let key = keys::document_key(db_id, coll_id, id)?;
        if self.read(&key)?.is_none() {
            return Ok(false);
        }
        self.write(key, None);
        Ok(true)
    }

    /// Every Document in the Collection that matches `filter`, in Document
    /// ID order. Scans the whole Collection.
    pub fn find(
        &mut self,
        database: &str,
        collection: &str,
        filter: &Document,
    ) -> Result<Vec<Document>> {
        let filter = Filter::parse(filter)?;
        let (db_id, coll_id) = self.resolve_collection(database, collection)?;
        let mut out = Vec::new();
        for bytes in self
            .scan(&keys::collection_prefix(db_id, coll_id))?
            .values()
        {
            let doc = document::decode(bytes)?;
            if filter.matches(&doc) {
                out.push(doc);
            }
        }
        Ok(out)
    }

    /// Commit. Returns the Commit Timestamp, or the Snapshot timestamp for a
    /// Transaction that wrote nothing.
    ///
    /// Aborts with [`Error::WriteConflict`] if a concurrent Transaction
    /// committed a write to any key this one wrote, or with
    /// [`Error::CollectionChanged`] if a Catalog entry this one used changed
    /// since its Snapshot. Both are retryable.
    pub fn commit(self) -> Result<u64> {
        let mut last_committed = self.db.last_committed.lock().expect("lock poisoned");
        let engine = &self.db.engine;
        let changed_since_snapshot = |key: &[u8]| -> Result<bool> {
            Ok(mvcc::latest_commit_ts(engine, key)?.is_some_and(|ts| ts > self.snapshot_ts))
        };

        for key in &self.catalog_reads {
            // Own Catalog writes are covered by the write-write check below.
            if !self.write_set.contains_key(key) && changed_since_snapshot(key)? {
                return Err(Error::CollectionChanged);
            }
        }
        for key in self.write_set.keys() {
            if changed_since_snapshot(key)? {
                return Err(Error::WriteConflict);
            }
        }
        if self.write_set.is_empty() {
            return Ok(self.snapshot_ts);
        }

        let commit_ts = *last_committed + 1;
        let mut batch = WriteBatch::new();
        for (key, value) in &self.write_set {
            batch.put(
                keys::physical_key(key, commit_ts),
                mvcc::encode_value(value.as_deref()),
            );
        }
        engine.write_batch(batch)?;
        *last_committed = commit_ts;
        Ok(commit_ts)
    }

    /// Abort, discarding the Write Set. Same as dropping the Transaction.
    pub fn abort(self) {}
}
