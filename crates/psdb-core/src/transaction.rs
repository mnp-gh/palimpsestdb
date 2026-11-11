//! Transactions: Snapshot reads, a private Write Set, and first-committer-
//! wins Snapshot Isolation at commit.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;

use bson::{Bson, Document, RawDocument};
use psdb_storage::{KeyRange, StorageEngine};

use crate::catalog::CollectionEntry;
use crate::collection_path::CollectionPath;
use crate::document;
use crate::error::{Error, Result};
use crate::filter::Filter;
use crate::index::{self, CandidateRange};
use crate::keys;
use crate::mvcc;
use crate::node::{CommitState, Node};

/// One entry of a Write Set: a logical key and its new value (`None`
/// deletes).
pub(crate) type Write<'a> = (&'a [u8], Option<&'a [u8]>);

/// Logical key → new value (`None` deletes), in key order.
pub(crate) type WriteSet = BTreeMap<Vec<u8>, Option<Vec<u8>>>;

/// Every entry of `write_set`, borrowed, in key order.
pub(crate) fn writes(write_set: &WriteSet) -> impl Iterator<Item = Write<'_>> {
    write_set.iter().map(|(k, v)| (k.as_slice(), v.as_deref()))
}

/// A Transaction: reads see the Snapshot taken at [`Node::begin`] plus
/// this Transaction's own writes; writes stay in its Write Set until
/// [`commit`](Self::commit).
///
/// Dropping a Transaction without committing aborts it.
pub struct Transaction<'node, E: StorageEngine> {
    pub(crate) node: &'node Node<E>,
    snapshot_ts: u64,
    /// Logical key → new value (`None` deletes). Ordered so scans can
    /// overlay it on the Snapshot.
    write_set: WriteSet,
    /// Bytes of every key and value in the Write Set, checked against
    /// [`Limits::max_write_set_bytes`](crate::Limits::max_write_set_bytes).
    write_set_bytes: usize,
    /// Catalog entries this Transaction depended on; checked at commit
    /// (ADR 0006). Added only through
    /// [`depend_on_catalog_entry`](Self::depend_on_catalog_entry).
    catalog_reads: BTreeSet<Vec<u8>>,
    /// Collections whose every Document a schema change or Index creation
    /// depended on (by key prefix). Added only through
    /// [`depend_on_whole_collection`](Self::depend_on_whole_collection).
    collection_reads: BTreeSet<Vec<u8>>,
}

impl<'node, E: StorageEngine> Transaction<'node, E> {
    pub(crate) fn new(node: &'node Node<E>, snapshot_ts: u64) -> Self {
        node.register_snapshot_at(snapshot_ts);
        Self::from_registered(node, snapshot_ts)
    }

    pub(crate) fn from_registered(node: &'node Node<E>, snapshot_ts: u64) -> Self {
        Self {
            node,
            snapshot_ts,
            write_set: WriteSet::new(),
            write_set_bytes: 0,
            catalog_reads: BTreeSet::new(),
            collection_reads: BTreeSet::new(),
        }
    }

    /// A Transaction whose Write Set is `write_set`, outside the Write Set
    /// cap: a chunk of an Index build (ADR 0010).
    pub(crate) fn with_write_set(
        node: &'node Node<E>,
        snapshot_ts: u64,
        write_set: WriteSet,
    ) -> Self {
        node.register_snapshot_at(snapshot_ts);
        Self {
            node,
            snapshot_ts,
            write_set,
            write_set_bytes: 0,
            catalog_reads: BTreeSet::new(),
            collection_reads: BTreeSet::new(),
        }
    }

    /// The Commit Timestamp this Transaction's Snapshot was taken at.
    pub fn snapshot_ts(&self) -> u64 {
        self.snapshot_ts
    }

    /// Record that this Transaction used the Catalog entry at `key`, so the
    /// commit aborts with Collection Changed if another commit changes it
    /// after the Snapshot (ADR 0006).
    pub(crate) fn depend_on_catalog_entry(&mut self, key: Vec<u8>) {
        self.catalog_reads.insert(key);
    }

    /// Record that this Transaction relied on every Document under
    /// `prefix`, so the commit aborts with Collection Changed if any
    /// Document there is committed after the Snapshot.
    pub(crate) fn depend_on_whole_collection(&mut self, prefix: Vec<u8>) {
        self.collection_reads.insert(prefix);
    }

    /// Read a logical key: own writes first, then the Snapshot.
    pub(crate) fn read(&self, logical: &[u8]) -> Result<Option<Vec<u8>>> {
        match self.write_set.get(logical) {
            Some(own) => Ok(own.clone()),
            None => mvcc::read_at(&self.node.engine, logical, self.snapshot_ts),
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
        if total > self.node.limits.max_write_set_bytes {
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
        mvcc::scan_range_at(&self.node.engine, range, self.snapshot_ts, &mut |key, _| {
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
        let mut live = BTreeMap::new();
        mvcc::scan_range_at(
            &self.node.engine,
            range,
            self.snapshot_ts,
            &mut |key, value| {
                live.insert(key.to_vec(), value);
                ControlFlow::Continue(())
            },
        )?;
        let own = self
            .write_set
            .range(range.start.clone()..)
            .take_while(|(k, _)| range.contains(k));
        for (key, value) in own {
            match value {
                Some(v) => live.insert(key.clone(), v.clone()),
                None => live.remove(key),
            };
        }
        Ok(live)
    }

    /// Insert a Document. If it has no `_id`, a UUIDv7 is generated.
    /// Returns the Document ID.
    pub fn insert(&mut self, collection: &CollectionPath, doc: Document) -> Result<Bson> {
        let entry = self.collection_entry(collection)?;
        let (id, bytes) = document::prepare(doc, None, &self.node.limits)?;
        let key = keys::document_key(entry.namespace_id, entry.id, &id)?;
        if self.read(&key)?.is_some() {
            return Err(Error::DuplicateId(id));
        }
        self.write_document(&entry, key, None, Some(bytes))?;
        Ok(id)
    }

    /// The Document with this Document ID, if it exists.
    pub fn get(&mut self, collection: &CollectionPath, id: &Bson) -> Result<Option<Document>> {
        let (ns_id, coll_id) = self.resolve_collection(collection)?;
        let key = keys::document_key(ns_id, coll_id, id)?;
        self.read(&key)?
            .map(|bytes| document::decode(&bytes))
            .transpose()
    }

    /// Replace the whole Document with this Document ID. `doc` may omit
    /// `_id`; if present it must equal `id`. Returns `false` if no such
    /// Document exists (nothing is written).
    pub fn replace(
        &mut self,
        collection: &CollectionPath,
        id: &Bson,
        doc: Document,
    ) -> Result<bool> {
        let entry = self.collection_entry(collection)?;
        let key = keys::document_key(entry.namespace_id, entry.id, id)?;
        let Some(old) = self.read(&key)? else {
            return Ok(false);
        };
        let (_, bytes) = document::prepare(doc, Some(id), &self.node.limits)?;
        self.write_document(&entry, key, Some(&old), Some(bytes))?;
        Ok(true)
    }

    /// Delete the Document with this Document ID. Returns `false` if no such
    /// Document exists.
    pub fn delete(&mut self, collection: &CollectionPath, id: &Bson) -> Result<bool> {
        let entry = self.collection_entry(collection)?;
        let key = keys::document_key(entry.namespace_id, entry.id, id)?;
        let Some(old) = self.read(&key)? else {
            return Ok(false);
        };
        self.write_document(&entry, key, Some(&old), None)?;
        Ok(true)
    }

    /// Write a Document's new Version (`None` deletes) together with the
    /// changes to every Index of its Collection, after checking Strict
    /// Fields and unique Indexes. Nothing is written if any check fails.
    fn write_document(
        &mut self,
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
        let doc_id = &key[entry.prefix().len()..];
        let mut writes = Vec::new();
        for ix in &entry.indexes {
            let index_entry = |doc: Option<&RawDocument>| match doc {
                Some(doc) => entry.index_entry(ix, doc, doc_id),
                None => Ok(None),
            };
            let (before, after) = (index_entry(old)?, index_entry(new_doc)?);
            // An unchanged value keeps its entry: it points to the Document
            // ID, not to a Version (ADR 0011).
            if before == after {
                continue;
            }
            if let Some(before) = before {
                writes.push((before.key, None));
            }
            if let Some(after) = after {
                if after.unique
                    && let Some(owner) = self.read(&after.key)?
                    && owner != doc_id
                {
                    return Err(Error::DuplicateValue(format!(
                        "another document already has this {} in the unique index on it",
                        ix.path
                    )));
                }
                writes.push((after.key, Some(doc_id.to_vec())));
            }
        }
        writes.push((key, new));
        self.write_all(writes)
    }

    /// Every Document in the Collection that matches `filter`, in Document
    /// ID order. Reads candidates from a range of `_id`s or from an Index
    /// when one fits the filter (see [`Node::create_index`]), and the
    /// whole Collection otherwise; either way every candidate is checked
    /// against the whole filter.
    pub fn find(
        &mut self,
        collection: &CollectionPath,
        filter: &Document,
    ) -> Result<Vec<Document>> {
        let filter = Filter::parse(filter)?;
        let entry = self.collection_entry(collection)?;
        let prefix = entry.prefix();
        let candidates = match index::plan(&filter, &entry) {
            None => self.scan_range(&entry.documents())?,
            Some(ranges) => {
                // Keyed by Document key, so a Document found through several
                // ranges is read once, and results stay in Document ID order.
                let mut docs = BTreeMap::new();
                for range in &ranges {
                    match range {
                        CandidateRange::Documents(range) => docs.extend(self.scan_range(range)?),
                        CandidateRange::Index(range) => {
                            for doc_id in self.scan_range(range)?.into_values() {
                                let key = [prefix.as_slice(), &doc_id].concat();
                                if !docs.contains_key(&key)
                                    && let Some(bytes) = self.read(&key)?
                                {
                                    docs.insert(key, bytes);
                                }
                            }
                        }
                    }
                }
                docs
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
    /// nothing always commits, unless the Node is poisoned.
    ///
    /// In a durable Node, fails with [`Error::TransactionTooLarge`] if
    /// its WAL Record would exceed 4 GiB, before anything is written. Fails
    /// with [`Error::CommitOutcomeUnknown`] if the WAL or the engine fails
    /// partway, which poisons the Node: every later commit, read-only
    /// or not, fails with [`Error::Poisoned`].
    pub fn commit(self) -> Result<u64> {
        let node = self.node;
        let mut commits = node.commit_lock.lock().expect("lock poisoned");
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
        let engine = &self.node.engine;
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
        commits.apply_commit(engine, &self.node.latest_visible_commit_ts, &self.write_set)
    }

    /// Abort, discarding the Write Set. Same as dropping the Transaction.
    pub fn abort(self) {}
}

impl<'node, E: StorageEngine> Drop for Transaction<'node, E> {
    fn drop(&mut self) {
        self.node.unregister_snapshot(self.snapshot_ts);
    }
}
