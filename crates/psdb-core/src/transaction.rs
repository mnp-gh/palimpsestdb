//! Transactions: Snapshot reads, a private Write Set, and first-committer-
//! wins Snapshot Isolation at commit.

use std::collections::{BTreeMap, BTreeSet};

use std::ops::ControlFlow;

use bson::{Bson, Document};
use psdb_storage::{KeyRange, StorageEngine};

use crate::catalog::CollectionEntry;
use crate::database::{CommitState, Database};
use crate::document;
use crate::error::{Error, Result};
use crate::filter::Filter;
use crate::index::{self, Source};
use crate::keys;
use crate::mvcc;

/// One entry of a Write Set: a logical key and its new value (`None`
/// deletes).
pub(crate) type Write<'a> = (&'a [u8], Option<&'a [u8]>);

/// Logical key → new value (`None` deletes), in key order.
pub(crate) type WriteSet = BTreeMap<Vec<u8>, Option<Vec<u8>>>;

/// Every entry of `write_set`, borrowed, in key order.
pub(crate) fn writes(write_set: &WriteSet) -> impl Iterator<Item = Write<'_>> {
    write_set.iter().map(|(k, v)| (k.as_slice(), v.as_deref()))
}

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
    pub(crate) write_set: WriteSet,
    /// Bytes of every key and value in the Write Set, checked against
    /// [`Limits::max_write_set_bytes`](crate::Limits::max_write_set_bytes).
    write_set_bytes: usize,
    /// Catalog entries this Transaction depended on; checked at commit
    /// (ADR 0006).
    pub(crate) catalog_reads: BTreeSet<Vec<u8>>,
    /// Collections whose every Document a schema change or Index creation
    /// depended on (by key prefix). Any Document committed into one since
    /// the Snapshot makes the commit abort with Collection Changed.
    pub(crate) collection_reads: BTreeSet<Vec<u8>>,
}

impl<'db, E: StorageEngine> Transaction<'db, E> {
    pub(crate) fn new(db: &'db Database<E>, snapshot_ts: u64) -> Self {
        Self {
            db,
            snapshot_ts,
            write_set: WriteSet::new(),
            write_set_bytes: 0,
            catalog_reads: BTreeSet::new(),
            collection_reads: BTreeSet::new(),
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

    pub(crate) fn write(&mut self, logical: Vec<u8>, value: Option<Vec<u8>>) -> Result<()> {
        self.write_all(vec![(logical, value)])
    }

    /// Add every write to the Write Set, or none of them if together they
    /// would cross the Write Set cap.
    fn write_all(&mut self, writes: Vec<(Vec<u8>, Option<Vec<u8>>)>) -> Result<()> {
        let size =
            |key: &[u8], value: &Option<Vec<u8>>| key.len() + value.as_ref().map_or(0, Vec::len);
        let mut total = self.write_set_bytes;
        for (key, value) in &writes {
            total += size(key, value);
            if let Some(old) = self.write_set.get(key) {
                total -= size(key, old);
            }
        }
        if total > self.db.limits.max_write_set_bytes {
            return Err(Error::TransactionTooLarge);
        }
        self.write_set.extend(writes);
        self.write_set_bytes = total;
        Ok(())
    }

    /// Whether any live value exists in `range`, in the Snapshot with own
    /// writes overlaid. Stops at the first one.
    pub(crate) fn any_live_in(&self, range: &KeyRange) -> Result<bool> {
        let own = self
            .write_set
            .range(range.start.clone()..)
            .take_while(|(k, _)| range.contains(k));
        if own.clone().any(|(_, v)| v.is_some()) {
            return Ok(true);
        }
        let mut found = false;
        mvcc::scan_range_at(&self.db.engine, range, self.snapshot_ts, &mut |key, _| {
            // Skip a value this Transaction deleted.
            if self.write_set.get(key) == Some(&None) {
                return ControlFlow::Continue(());
            }
            found = true;
            ControlFlow::Break(())
        })?;
        Ok(found)
    }

    /// Every live value under `prefix`: the Snapshot with own writes
    /// overlaid, in key order.
    pub(crate) fn scan(&self, prefix: &[u8]) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
        self.scan_range(&KeyRange::prefix(prefix))
    }

    /// Every live value whose logical key is in `range`: the Snapshot with
    /// own writes overlaid, in key order.
    pub(crate) fn scan_range(&self, range: &KeyRange) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
        let mut rows = BTreeMap::new();
        mvcc::scan_range_at(
            &self.db.engine,
            range,
            self.snapshot_ts,
            &mut |key, value| {
                rows.insert(key.to_vec(), value);
                ControlFlow::Continue(())
            },
        )?;
        let own = self
            .write_set
            .range(range.start.clone()..)
            .take_while(|(k, _)| range.contains(k));
        for (key, value) in own {
            match value {
                Some(v) => rows.insert(key.clone(), v.clone()),
                None => rows.remove(key),
            };
        }
        Ok(rows)
    }

    /// Insert a Document. If it has no `_id`, a UUIDv7 is generated.
    /// Returns the Document ID.
    pub fn insert(&mut self, database: &str, collection: &str, doc: Document) -> Result<Bson> {
        let (db_id, entry) = self.collection_entry(database, collection)?;
        let (id, bytes) = document::prepare(doc, None, &self.db.limits)?;
        let key = keys::document_key(db_id, entry.id, &id)?;
        if self.read(&key)?.is_some() {
            return Err(Error::DuplicateId(id.to_string()));
        }
        self.write_document(db_id, &entry, key, None, Some(bytes))?;
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
        let (db_id, entry) = self.collection_entry(database, collection)?;
        let key = keys::document_key(db_id, entry.id, id)?;
        let Some(old) = self.read(&key)? else {
            return Ok(false);
        };
        let (_, bytes) = document::prepare(doc, Some(id), &self.db.limits)?;
        self.write_document(db_id, &entry, key, Some(&old), Some(bytes))?;
        Ok(true)
    }

    /// Delete the Document with this Document ID. Returns `false` if no such
    /// Document exists.
    pub fn delete(&mut self, database: &str, collection: &str, id: &Bson) -> Result<bool> {
        let (db_id, entry) = self.collection_entry(database, collection)?;
        let key = keys::document_key(db_id, entry.id, id)?;
        let Some(old) = self.read(&key)? else {
            return Ok(false);
        };
        self.write_document(db_id, &entry, key, Some(&old), None)?;
        Ok(true)
    }

    /// Write a Document's new Version (`None` deletes) together with the
    /// changes to every Index of its Collection, after checking Strict
    /// Fields and unique Indexes. Nothing is written if any check fails.
    fn write_document(
        &mut self,
        db_id: u32,
        entry: &CollectionEntry,
        key: Vec<u8>,
        old: Option<&[u8]>,
        new: Option<Vec<u8>>,
    ) -> Result<()> {
        let old = old.map(document::raw).transpose()?;
        let new_doc = new.as_deref().map(document::raw).transpose()?;
        if let Some(doc) = new_doc {
            entry.check_strict(doc)?;
        }
        let doc_id = &key[keys::collection_prefix(db_id, entry.id).len()..];
        let mut writes = Vec::new();
        for ix in &entry.indexes {
            let segments = entry.segments(&ix.path)?;
            let ty = entry.schema[&ix.path].field_type;
            let component = |doc: Option<&bson::RawDocument>| match doc {
                Some(doc) => index::component_of(doc, segments, ty),
                None => Ok(None),
            };
            let (before, after) = (component(old)?, component(new_doc)?);
            // An unchanged value keeps its entry: it points to the Document
            // ID, not to a Version (ADR 0011).
            if before == after {
                continue;
            }
            if let Some(c) = before {
                writes.push((index::entry_key(db_id, entry.id, ix, &c, doc_id), None));
            }
            if let Some(c) = after {
                let entry_key = index::entry_key(db_id, entry.id, ix, &c, doc_id);
                if ix.unique
                    && !keys::is_null(&c)
                    && let Some(owner) = self.read(&entry_key)?
                    && owner != doc_id
                {
                    return Err(Error::DuplicateValue(format!(
                        "another document already has this {} in the unique index on it",
                        ix.path
                    )));
                }
                writes.push((entry_key, Some(doc_id.to_vec())));
            }
        }
        writes.push((key, new));
        self.write_all(writes)
    }

    /// Every Document in the Collection that matches `filter`, in Document
    /// ID order. Reads candidates from a range of `_id`s or from an Index
    /// when one fits the filter (see [`Database::create_index`]), and the
    /// whole Collection otherwise; either way every candidate is checked
    /// against the whole filter.
    pub fn find(
        &mut self,
        database: &str,
        collection: &str,
        filter: &Document,
    ) -> Result<Vec<Document>> {
        let filter = Filter::parse(filter)?;
        let (db_id, entry) = self.collection_entry(database, collection)?;
        let prefix = keys::collection_prefix(db_id, entry.id);
        let candidates = match index::plan(&filter, db_id, entry.id, &entry.schema, &entry.indexes)
        {
            None => self.scan_range(&keys::documents_range(db_id, entry.id))?,
            Some(sources) => {
                // Keyed by Document key, so a Document found through several
                // ranges is read once, and results stay in Document ID order.
                let mut rows = BTreeMap::new();
                for source in &sources {
                    match source {
                        Source::Documents(range) => rows.extend(self.scan_range(range)?),
                        Source::Index(range) => {
                            for doc_id in self.scan_range(range)?.into_values() {
                                let key = [prefix.as_slice(), &doc_id].concat();
                                if !rows.contains_key(&key)
                                    && let Some(bytes) = self.read(&key)?
                                {
                                    rows.insert(key, bytes);
                                }
                            }
                        }
                    }
                }
                rows
            }
        };
        let mut out = Vec::new();
        for bytes in candidates.values() {
            if filter.matches_raw(document::raw(bytes)?)? {
                out.push(document::decode(bytes)?);
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
    /// since its Snapshot. Both are retryable. A Transaction that wrote
    /// nothing always commits, unless the Database is poisoned.
    ///
    /// In a durable Database, fails with [`Error::TransactionTooLarge`] if
    /// its WAL Record would exceed 4 GiB, before anything is written. Fails
    /// with [`Error::CommitOutcomeUnknown`] if the WAL or the engine fails
    /// partway, which poisons the Database: every later commit, read-only
    /// or not, fails with [`Error::Poisoned`].
    pub fn commit(self) -> Result<u64> {
        let db = self.db;
        let mut commits = db.commits.lock().expect("lock poisoned");
        self.commit_locked(&mut commits)
    }

    /// [`commit`](Self::commit), for a caller already holding the commit
    /// lock.
    pub(crate) fn commit_locked(self, commits: &mut CommitState) -> Result<u64> {
        if commits.poisoned {
            return Err(Error::Poisoned);
        }
        // A read-only Transaction saw a consistent Snapshot and wrote nothing
        // that could be lost, so it commits even if a Collection it read was
        // dropped since.
        if self.write_set.is_empty() {
            return Ok(self.snapshot_ts);
        }
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
        // Reads every Version ever written under the Collection, while
        // holding the commit lock: rare (strict-field changes and Index
        // creation on empty Collections) and bounded once garbage
        // collection exists (v0.4).
        for prefix in &self.collection_reads {
            if mvcc::changed_since(engine, prefix, self.snapshot_ts)? {
                return Err(Error::CollectionChanged);
            }
        }
        for key in self.write_set.keys() {
            if changed_since_snapshot(key)? {
                return Err(Error::WriteConflict);
            }
        }
        self.db.apply_commit(commits, &self.write_set)
    }

    /// Abort, discarding the Write Set. Same as dropping the Transaction.
    pub fn abort(self) {}
}
