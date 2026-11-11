# v0.4: Concurrency

Status: ready-for-agent

A thread-safe Database Layer for embedded use with plain threads: Transactions begin and run in parallel, commits stay one at a time, Index builds no longer stop the world, and old Versions are collected. Terms follow [GLOSSARY.md](../../GLOSSARY.md); the design is [ADR 0004](../../docs/adr/0004-database-layer-owns-wal.md), [ADR 0007](../../docs/adr/0007-runtime-model.md), [ADR 0012](../../docs/adr/0012-garbage-collection-is-not-logged.md) and [ADR 0013](../../docs/adr/0013-building-indexes-take-the-lock-per-chunk.md).

## Out of scope

- `fjall`, streaming scans, and Recovery from `applied_up_to` (v0.4.1).
- Splitting the commit into a pipeline; group commit (v0.6).
- Running Garbage Collection automatically (later; v0.4 has only `Node::collect_garbage()`).
- A range-delete operation in the Storage Engine (when an engine benefits, likely v0.9).
- Resuming an Index build after a crash.
- Bulk writes larger than the Write Set cap; clients split them (Unscheduled).
- The async front end (v0.6).

## Threads

- `Node` is `Send + Sync`, shared across threads by `Arc` or scoped threads; a `Transaction` is `Send` and used by one thread at a time. A compile-time assertion checks both for `Node<MemoryEngine>`.
- One commit lock still covers the conflict check, WAL append and fsync, and the apply. Its holder is the single in-order applier (ADR 0004).
- A panic while holding the commit lock poisons the Rust `Mutex`; every later commit panics too, and the Node must be reopened (Recovery repairs a half-applied commit from the WAL). Turning this into an `Error` waits for the server (v0.6). `begin_transaction()` checks whether the lock is poisoned and fails fast.
- Visibility watermark: `latest_visible_commit_ts: AtomicU64`, stored with `Release` after the apply, still under the commit lock. `begin_transaction()` never takes the commit lock.
- Snapshot registry: `begin_transaction()` takes a small registry lock, loads `latest_visible_commit_ts` with `Acquire`, registers that Snapshot and releases the lock; dropping the Transaction (committed, aborted or abandoned) unregisters it via `Drop`. The Horizon is the oldest registered Snapshot, or `latest_visible_commit_ts` when none is, read under the same lock, so no Snapshot can register below a Horizon already handed out.

## Catalog cache

- Holds the newest Version of each Catalog entry with its Commit Timestamp, updated under the commit lock when a commit writes Catalog entries (Index build chunks write none). Drops are retained as tombstones and purged once the Horizon passes their Commit Timestamp during Garbage Collection.
- A Transaction whose Snapshot is at or after an entry's timestamp reads it from the cache; an older Snapshot reads the engine. A Transaction's own Write Set still wins over both.
- Catalog dependency checks at commit are unchanged.
- Caches Namespace and Collection entries only; ID keys stay engine-only, since `allocate_id` asks whether any Version ever existed, not what a Snapshot sees.
- Serves point reads only (`resolve_namespace`, `collection_entry`). Listings keep scanning the engine.
- Holds raw bytes, `RwLock<HashMap<logical key, (Commit Timestamp, Option<bytes>)>>` on `Node`, beside the commit lock rather than inside `CommitState`, so readers never wait on a commit. The lookup sits inside `Transaction::read`; callers are unchanged.
- The cache is complete, so a miss means absent at every Snapshot (a purged Tombstone is below the Horizon) and needs no engine read.
- Commit order: engine write, then cache update, then publish `latest_visible_commit_ts`. Publishing first would let a Snapshot at the new timestamp read the old cached entry. Comment this where it is enforced; testing the race waits for the simulator (issue 06).
- Filled on open by feeding each replayed WAL Record to the same function commits use.
- Tests: in debug builds every cache answer is cross-checked against the engine with `debug_assert_eq!`. Plus: an old Snapshot still sees a Collection dropped after it began; a never-created Namespace reads as absent; a Transaction's own write beats the cache.

## Garbage Collection

- `Node::collect_garbage()` takes the Horizon, scans versioned keys in batches (up to 1,024 physical keys per `WriteBatch`) starting at `NAMESPACES_COLLECTION` (bypassing unversioned `STATE_COLLECTION`), without the commit lock, and for each logical key deletes every Version older than the newest Version at or below the Horizon. If that newest Version is a tombstone, it is deleted too. Returns `GcStats`.
- One pass at a time: a GC mutex serialises `collect_garbage()` calls, since a later pass could delete Catalog Versions an earlier pass is still reading at its older Horizon. It never blocks commits or `begin_transaction()`.
- Deletes go straight to the engine and are not logged (ADR 0012).
- Reclaim reads the Catalog at the Horizon, never at the newest Commit Timestamp, so nothing created after the Horizon is ever taken for dead. A Collection is dead when its ID key is visible at the Horizon but no Collection entry names that ID; an Index is dead when its ID is below its Collection's `nextIndexId` at the Horizon but not in that entry's `indexes` (dropped, failed, or dropped by Recovery). IDs are never reused, so dead stays dead. Each dead ID's key range is scanned and its keys deleted in batches.
- After reclaim, the commit-time check for strict-field changes and Index creation on empty Collections reads only live data.

## Index builds

- `Node::create_index` first commits the Index to the Catalog entry as Building (`state` in `indexes`; entries written before v0.4 read as Ready). That commit takes the Index ID and moves `nextIndexId` past it, so every chunk's entries sit below the counter where Reclaim can find them if the build fails.
- From that commit on, every write maintains a Building Index's entries and enforces its uniqueness (Duplicate Value); `find` plans only with Ready Indexes. A Transaction whose Snapshot predates the Building commit aborts with Collection Changed through the existing dependency on the Collection's Catalog entry; this is what makes skipping a catch-up pass safe (ADR 0013), so a test must cover it.
- The build then takes the commit lock per chunk (1024 Documents or 16 MiB), reads that chunk at the newest Commit Timestamp, resuming after the last key it built, and commits its entries. A final commit marks the Index Ready. The calling thread runs the build; other threads commit between chunks.
- A Building Index counts as its field's Index: a second `create_index` fails with Index Exists. Dropping the Collection makes the next chunk stop with Collection Changed; dropping the Index cancels the build the same way. A duplicate found by a unique build fails it with Duplicate Value and one commit removes the Catalog entry.
- Recovery drops every Index still Building, in a commit, when the Node opens.

## Seams and simulator

- Clock seam for UUIDv7 Document IDs, and a seeded random source for Catalog IDs, both injectable through `Node::open_with`.
- A deterministic simulator: a seeded, single-threaded scheduler interleaving steps of many Transactions, Index build chunks and Garbage Collection passes over `SimDisk`, checking invariants after every step: each Snapshot's reads never change, Indexes agree with their Documents at every Snapshot, and crash plus Recovery keeps every acknowledged commit.
- A stress test on real threads (not deterministic) to catch data races in the locking.

## Tests

- Unit tests for the registry and horizon, the cache's fallback to the engine, and collection of tombstones.
- Integration tests: a long build while other threads commit; drop during a build; crash during a build; Garbage Collection never removing a Version an open Snapshot reads.
- The simulator and the stress test above.

## CLI

`gc` runs `Node::collect_garbage()` and prints how many Versions it removed. `indexes` shows Building or Ready.
