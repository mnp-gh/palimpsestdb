//! Transactions: Snapshot reads, a private Write Set, and first-committer-
//! wins Snapshot Isolation at commit.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;
use std::sync::atomic::Ordering;

use bson::{Bson, Document, RawDocument};
use psdb_storage::{KeyRange, StorageEngine};

use crate::catalog::{Cached, CollectionEntry};
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

/// A Transaction: reads see the Snapshot taken at [`Node::begin_transaction`] plus
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
    /// Keys of the Catalog entries this Transaction used: Namespaces, and
    /// Collections with their schema and Indexes. At commit, one point
    /// lookup per key; a newer Version aborts with Collection Changed
    /// (ADR 0006). Added only through
    /// [`depend_on_catalog_entry`](Self::depend_on_catalog_entry).
    catalog_entry_reads: BTreeSet<Vec<u8>>,
    /// Key prefixes of Collections whose Documents and Index entries a
    /// schema change or Index creation relied on. The Catalog entry is not
    /// under the prefix, so it is covered by `catalog_entry_reads`. At
    /// commit, a Document committed into one after the Snapshot aborts with
    /// Collection Changed. Added only through
    /// [`depend_on_whole_collection`](Self::depend_on_whole_collection).
    collection_content_reads: BTreeSet<Vec<u8>>,
}

impl<'node, E: StorageEngine> Transaction<'node, E> {
    /// A Transaction whose Snapshot is the latest Visible commit.
    pub(crate) fn new(node: &'node Node<E>) -> Self {
        Self {
            node,
            snapshot_ts: node.register_snapshot(),
            write_set: WriteSet::new(),
            write_set_bytes: 0,
            catalog_entry_reads: BTreeSet::new(),
            collection_content_reads: BTreeSet::new(),
        }
    }

    /// A Transaction whose Write Set is `write_set`, outside the Write Set
    /// cap: a chunk of an Index build (ADR 0010).
    pub(crate) fn with_write_set(node: &'node Node<E>, write_set: WriteSet) -> Self {
        let mut txn = Self::new(node);
        txn.write_set = write_set;
        txn
    }

    /// The Commit Timestamp this Transaction's Snapshot was taken at.
    pub fn snapshot_ts(&self) -> u64 {
        self.snapshot_ts
    }

    /// Record that this Transaction used the Catalog entry at `key`, so the
    /// commit aborts with Collection Changed if another commit changes it
    /// after the Snapshot (ADR 0006).
    pub(crate) fn depend_on_catalog_entry(&mut self, key: Vec<u8>) {
        self.catalog_entry_reads.insert(key);
    }

    /// Record that this Transaction relied on every Document under
    /// `prefix`, so the commit aborts with Collection Changed if any
    /// Document there is committed after the Snapshot.
    pub(crate) fn depend_on_whole_collection(&mut self, prefix: Vec<u8>) {
        self.collection_content_reads.insert(prefix);
    }

    /// Read a logical key: own writes first, then the Snapshot, from the
    /// Catalog cache when it has the answer.
    pub(crate) fn read(&self, logical: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some(own) = self.write_set.get(logical) {
            return Ok(own.clone());
        }
        if keys::is_catalog_entry_key(logical)
            && let Cached::Known(cached) =
                self.node.catalog_cache.read_at(logical, self.snapshot_ts)
        {
            // Debug builds check the cache against the engine; release builds skip the read.
            debug_assert_eq!(
                cached,
                mvcc::read_at(&self.node.engine, logical, self.snapshot_ts)?,
                "Catalog cache disagrees with the engine"
            );
            return Ok(cached);
        }
        mvcc::read_at(&self.node.engine, logical, self.snapshot_ts)
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
        let mut own = self
            .write_set
            .range(range.start.clone()..)
            .take_while(|(k, _)| range.contains(k));
        if own.any(|(_, v)| v.is_some()) {
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
    /// overlaid, in key order. Catalog listings come from the Catalog cache
    /// when it has the answer.
    pub(crate) fn scan(&self, prefix: &[u8]) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
        let range = KeyRange::prefix(prefix);
        if keys::is_catalog_entry_key(prefix)
            && let Some(cached) = self.node.catalog_cache.scan_at(prefix, self.snapshot_ts)
        {
            debug_assert_eq!(
                cached,
                self.scan_snapshot(&range)?,
                "Catalog cache disagrees with the engine"
            );
            return Ok(self.overlay_own_writes(&range, cached));
        }
        let live = self.scan_snapshot(&range)?;
        Ok(self.overlay_own_writes(&range, live))
    }

    /// Every live value in `range` at the Snapshot, read from the engine.
    fn scan_snapshot(&self, range: &KeyRange) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
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
        Ok(live)
    }

    /// `live` with this Transaction's writes in `range` laid over it.
    fn overlay_own_writes(
        &self,
        range: &KeyRange,
        mut live: BTreeMap<Vec<u8>, Vec<u8>>,
    ) -> BTreeMap<Vec<u8>, Vec<u8>> {
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
        live
    }

    /// Every live key and value in `range`: the Snapshot with own writes
    /// overlaid, in key order, read from the engine a chunk at a time
    /// (ADR 0016).
    pub(crate) fn scan_range(
        &self,
        range: &KeyRange,
    ) -> impl Iterator<Item = Result<(Vec<u8>, Vec<u8>)>> + '_ {
        let mut scan = Scan::new(range);
        std::iter::from_fn(move || scan.next(self))
    }

    /// Insert a Document. If it has no `_id`, a UUIDv7 is generated.
    /// Returns the Document ID.
    pub fn insert(&mut self, collection: &CollectionPath, doc: Document) -> Result<Bson> {
        let entry = self.collection_entry(collection)?;
        let (id, bytes) = document::prepare(doc, None, &self.node.limits, &self.node.id_generator)?;
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
        let (_, bytes) =
            document::prepare(doc, Some(id), &self.node.limits, &self.node.id_generator)?;
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
            // During a build a unique value can still have two owners; keep the
            // other Document's entry. No entry at the Snapshot may be one a
            // chunk wrote since, so remove it and let the conflict check
            // decide (ADR 0013).
            if let Some(before) = before
                && (!before.unique || self.read(&before.key)?.is_none_or(|owner| owner == doc_id))
            {
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
    ///
    /// The Documents stream: they are read from the engine a chunk at a
    /// time as the iterator advances (ADR 0016), so the Transaction stays
    /// borrowed (`'txn`) until the iterator is dropped. An Index plan first collects
    /// the keys of every candidate Document. After an error the iterator
    /// ends.
    pub fn find<'txn>(
        &'txn mut self,
        collection: &CollectionPath,
        filter: &Document,
    ) -> Result<impl Iterator<Item = Result<Document>> + 'txn> {
        let filter = Filter::parse(filter)?;
        let entry = self.collection_entry(collection)?;
        // From here on only reads, so the iterator can borrow `self` shared.
        let this = &*self;
        let mut candidates: Box<dyn Iterator<Item = Result<Vec<u8>>> + 'txn> =
            match index::plan(&filter, &entry) {
                None => {
                    let documents = this.scan_range(&entry.documents());
                    Box::new(documents.map(|item| item.map(|(_, bytes)| bytes)))
                }
                Some(ranges) => {
                    let keys = this.candidate_keys(&entry.prefix(), &ranges)?;
                    // A key whose Document is gone (`read` gives `None`) is skipped.
                    Box::new(
                        keys.into_iter()
                            .filter_map(|key| this.read(&key).transpose()),
                    )
                }
            };
        Ok(std::iter::from_fn(move || {
            while let Some(bytes) = candidates.next() {
                match bytes.and_then(|bytes| Self::decode_if_matches(&filter, &bytes)) {
                    Ok(None) => {}
                    Ok(Some(doc)) => return Some(Ok(doc)),
                    Err(e) => {
                        // End the iterator after its first error.
                        candidates = Box::new(std::iter::empty());
                        return Some(Err(e));
                    }
                }
            }
            None
        }))
    }

    /// The keys of the Documents an Index plan's `ranges` name. A set, so a
    /// Document found through several ranges is read once, in Document ID
    /// order.
    fn candidate_keys(
        &self,
        prefix: &[u8],
        ranges: &[CandidateRange],
    ) -> Result<BTreeSet<Vec<u8>>> {
        let mut keys = BTreeSet::new();
        for range in ranges {
            match range {
                CandidateRange::Documents(range) => {
                    for item in self.scan_range(range) {
                        let (key, _) = item?;
                        keys.insert(key);
                    }
                }
                CandidateRange::Index(range) => {
                    for item in self.scan_range(range) {
                        let (_, doc_id) = item?;
                        keys.insert([prefix, &doc_id].concat());
                    }
                }
            }
        }
        Ok(keys)
    }

    /// The decoded Document in `bytes` if it matches `filter`, else `None`.
    fn decode_if_matches(filter: &Filter, bytes: &[u8]) -> Result<Option<Document>> {
        if !filter.matches_raw(document::raw(bytes)?)? {
            return Ok(None);
        }
        document::decode(bytes).map(Some)
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
        // Read-only: skip the lock, so it never waits behind another commit's fsync.
        if self.write_set.is_empty() && !node.poisoned.load(Ordering::Relaxed) {
            return Ok(self.snapshot_ts);
        }
        let mut commit_state = node.commit_lock.lock().expect("lock poisoned");
        self.commit_under_lock(&mut commit_state)
    }

    /// [`commit`](Self::commit), for a caller already holding the commit
    /// lock.
    pub(crate) fn commit_under_lock(self, commit_state: &mut CommitState) -> Result<u64> {
        if self.node.poisoned.load(Ordering::Relaxed) {
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

        for key in &self.catalog_entry_reads {
            // Own Catalog writes are covered by the write-write check below.
            if !self.write_set.contains_key(key) && changed_since_snapshot(key)? {
                return Err(Error::CollectionChanged);
            }
        }
        // shortcut: on a Collection that gets Documents all the time, a
        // Strict change can abort every retry, since one lands while the
        // existing Documents are checked; upgrade to the gradual design
        // (.scratch/v1.0-hardening/issues/01-gradual-strict-field.md) if that bites.
        for prefix in &self.collection_content_reads {
            // No entry: no Document written since the Node opened, which
            // precedes every Snapshot.
            if commit_state
                .collection_updated_at
                .get(prefix)
                .is_some_and(|&ts| ts > self.snapshot_ts)
            {
                return Err(Error::CollectionChanged);
            }
        }
        for key in self.write_set.keys() {
            if changed_since_snapshot(key)? {
                return Err(Error::WriteConflict);
            }
        }
        commit_state.apply_commit(
            engine,
            &self.node.catalog_cache,
            &self.node.latest_visible_commit_ts,
            &self.node.poisoned,
            &self.write_set,
        )
    }

    /// Abort, discarding the Write Set. Same as dropping the Transaction.
    pub fn abort(self) {}
}

impl<E: StorageEngine> Drop for Transaction<'_, E> {
    fn drop(&mut self) {
        self.node.unregister_snapshot(self.snapshot_ts);
    }
}

/// Where a chunked scan of a key range stands (ADR 0016). It holds no
/// borrow, so each step is handed the Transaction it reads through.
struct Scan {
    /// Where the next chunk starts, or `None` once the range is done or a
    /// chunk failed.
    next_start: Option<Vec<u8>>,
    end: Option<Vec<u8>>,
    chunk: std::collections::btree_map::IntoIter<Vec<u8>, Vec<u8>>,
}

impl Scan {
    fn new(range: &KeyRange) -> Self {
        Self {
            next_start: Some(range.start.clone()),
            end: range.end.clone(),
            chunk: BTreeMap::new().into_iter(),
        }
    }

    /// The next live key and value. After the range ends or an error,
    /// `None`.
    fn next<E: StorageEngine>(
        &mut self,
        txn: &Transaction<'_, E>,
    ) -> Option<Result<(Vec<u8>, Vec<u8>)>> {
        loop {
            if let Some(item) = self.chunk.next() {
                return Some(Ok(item));
            }
            let start = self.next_start.take()?;
            let range = KeyRange {
                start,
                end: self.end.clone(),
            };
            if let Err(e) = self.read_chunk(txn, range) {
                return Some(Err(e));
            }
        }
    }

    /// Read the next chunk of `range` at the Snapshot with own writes
    /// overlaid, and set where the chunk after it starts.
    fn read_chunk<E: StorageEngine>(
        &mut self,
        txn: &Transaction<'_, E>,
        range: KeyRange,
    ) -> Result<()> {
        let limits = &txn.node.limits;
        let mut live = BTreeMap::new();
        let mut bytes = 0;
        let mut last = None;
        mvcc::scan_range_at(
            &txn.node.engine,
            &range,
            txn.snapshot_ts,
            &mut |key, value| {
                bytes += value.len();
                live.insert(key.to_vec(), value);
                if live.len() >= limits.scan_chunk_keys || bytes >= limits.scan_chunk_bytes {
                    last = Some(key.to_vec());
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(())
            },
        )?;
        // The part of `range` this chunk covers, for overlaying own writes.
        let covered = match last {
            None => range,
            Some(last) => {
                // Just past the oldest possible Version of `last`; keys are
                // self-delimiting (ADR 0008), so no other logical key falls between.
                let mut resume = keys::physical_key(&last, 0);
                resume.push(0);
                self.next_start = Some(resume.clone());
                KeyRange::new(range.start, resume)
            }
        };
        self.chunk = txn.overlay_own_writes(&covered, live).into_iter();
        Ok(())
    }
}

/// Every entry of `write_set`, borrowed, in key order.
pub(crate) fn writes(write_set: &WriteSet) -> impl Iterator<Item = Write<'_>> {
    write_set.iter().map(|(k, v)| (k.as_slice(), v.as_deref()))
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use psdb_storage_memory::MemoryEngine;

    use crate::Node;

    #[test]
    fn read_only_commit_does_not_wait_for_the_commit_lock() {
        let node = Node::new(MemoryEngine::new()).unwrap();
        let held = node.commit_lock.lock().unwrap();
        let (done, finished) = mpsc::channel();
        thread::scope(|s| {
            s.spawn(|| done.send(node.begin_transaction().commit()).unwrap());
            let committed = finished.recv_timeout(Duration::from_secs(5));
            drop(held);
            assert!(matches!(committed, Ok(Ok(_))), "read-only commit blocked");
        });
    }
}
