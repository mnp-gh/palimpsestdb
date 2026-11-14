# Architecture

A map of the code: what lives where, how a request flows through it, and the rules that hold across modules. It describes v0.4 in progress: an embedded library, thread-safe for plain threads, durable through a WAL over an in-memory Storage Engine.

Read [GLOSSARY.md](GLOSSARY.md) first; capitalised terms here are defined there. The *why* behind each decision is in [docs/adr/](docs/adr/), and the details are in each module's doc comments (`cargo doc --open --document-private-items`). This file only connects them, so it names modules and types, never lines.

## Bird's-eye view

```
psdb-cli ──────► psdb-core ──────► psdb-storage ◄────── psdb-storage-memory
(the shell)      (Database Layer)  (StorageEngine trait)  (BTreeMap engine)
```

- **`psdb-storage`**: the `StorageEngine` trait (ordered byte keys to byte values, atomic batches, range scans) and its conformance suite. It knows nothing about Documents, Versions or Transactions (ADR 0001).
- **`psdb-storage-memory`**: the only engine until v0.9. Nothing it holds survives a restart; the WAL rebuilds it.
- **`psdb-core`**: the Database Layer, everything that gives bytes meaning. Generic over the engine: `Node<E: StorageEngine>`.
- **`psdb-cli`**: the `psdb` shell, the only binary. It picks the engine.

Inside `psdb-core`, from the public API down:

```
Node, Transaction                         public API; commits
  gc                                      removing Versions below the Horizon
  catalog, index, schema, filter,         what keys and values mean
  document, collection_path, field_path
    keys, mvcc                            key encoding; which Version a Snapshot sees
      StorageEngine                       bytes
wal (beside the stack)                    the log every commit goes through first
```

Modules are cut by concept, not by type. `Node`, `Transaction` and `CollectionEntry` are defined in `node`, `transaction` and `catalog`, but their methods about one concept live in that concept's module, in an `impl` block there: `Transaction::create_index_on_empty` and `Node::create_index` are in `index`, `Transaction::set_field_type` in `schema`. So `catalog` holds only Catalog entries and Namespace and Collection changes.

## Where to look

| Question | Module |
|---|---|
| How is a key laid out? | `keys` (ADR 0008) |
| Which Version does a Snapshot see? | `mvcc` |
| How does a commit get checked and logged? | `transaction` (`commit_under_lock`), `node` (`CommitState`) |
| What does a Catalog entry hold? | `catalog` (`CollectionEntry`) |
| What may change in a Collection Schema? | `schema` |
| How is an Index created, built, or kept in step with a write? | `index` (`index_entry`, `create_index`, `create_index_on_empty`, `IndexBuild::step`) |
| Which Index or `_id` range does `find` use? | `index` (`plan`) |
| What is a valid name, `_id` or Document? | `document`, `collection_path`, `field_path` |
| How does recovery work? | `node` (`open_with`), `index` (`drop_building_indexes`), `wal` |

## Keys

Every stored thing is a **logical key** with Versions. A **physical key** is the logical key followed by the inverted Commit Timestamp, so a key's newest Version sorts first (ADR 0008).

- A Document: `namespace_id ‖ collection_id ‖ document_id`.
- An Index entry: `namespace_id ‖ collection_id ‖ 0x00 ‖ index_id ‖ value ‖ document_id`, below every Document of its Collection. A unique Index leaves out the `document_id` (ADR 0011).
- The Catalog: Collections 1 to 4 of Namespace 0 (ADR 0006). Its Collection 0 holds unversioned state: the `applied_up_to` marker.

So everything a Collection owns sits under one prefix, `namespace_id ‖ collection_id`.

## Flows

**Opening.** `Node::open` opens the WAL file and locks it, so a second Node on the same directory fails to open. `Wal::open` reads every record, truncates a torn one at the end, and fsyncs the file, so no record it returns can be lost to a later power cut. `Node::open_with` writes each record into the empty engine with `mvcc::commit_batch` and records its Catalog entries in the Catalog cache, and the last record's Commit Timestamp becomes the next Snapshot. `Node::new` skips all of this and keeps nothing durable; it too refuses an engine that holds data.

**Beginning** (`Node::begin_transaction`) never takes the commit lock. Under the Snapshot registry's own lock it loads `latest_visible_commit_ts` and registers that Snapshot; dropping the Transaction unregisters it. `Node::horizon` reads the oldest registered Snapshot under the same lock, so no Snapshot registers below a Horizon already handed out.

**A write** (`Transaction::insert`, `replace`, `delete`) touches only the Transaction's private Write Set:

1. `collection_entry` reads the Collection's Catalog entry, which holds its schema and Indexes, and records the read as a dependency.
2. `document::prepare` checks the limits and the `_id`, generating a UUIDv7 if there is none.
3. `write_document` checks Strict Fields. Then, for every Index, `CollectionEntry::index_entry` works out the old and new entries, and a unique Index refuses a value another Document owns.
4. `write_all` adds the Document and its Index entry changes to the Write Set together, or adds none of them if they would cross the Write Set cap.

**A read** (`Transaction::read`, `scan_range`) sees the Snapshot with the Transaction's own writes laid over it. A point read of a Namespace or Collection entry asks `Node::catalog_cache` first: it holds each entry's newest Version, so it answers when that Version is at or before the Snapshot, or when the key was never written; only an entry changed since the Snapshot goes to the engine. Debug builds check every cached answer against the engine. Listings and ID keys always read the engine. `find` parses the filter and asks `index::plan` for candidate ranges. With no plan, it scans the Collection's Documents. A `Documents` range is read directly. An `Index` range yields Document IDs, which are then fetched. Every candidate is rechecked against the whole filter (`Filter::matches_raw`).

**A commit** (`Transaction::commit`) takes the commit lock (`Node::commit_lock`), then:

1. Fails if the Node is poisoned. A Transaction that wrote nothing commits at once.
2. Aborts with Collection Changed if a Catalog entry it read, or a Collection it read whole, changed since its Snapshot.
3. Aborts with Write Conflict if any key it writes was committed since its Snapshot (first-committer-wins).
4. Calls `CommitState::apply_commit`, which takes the next Commit Timestamp, appends and fsyncs the WAL record, then writes one engine batch: every Version plus `applied_up_to`. A failure after the append poisons the Node with Commit Outcome Unknown (ADR 0004).
5. `apply_commit` then records the commit's Catalog entries in `Node::catalog_cache`, and only then stores the Commit Timestamp in `Node::latest_visible_commit_ts`, the only copy of it, which makes the commit Visible.

**Catalog changes** (creating or dropping Namespaces and Collections, schema and Index changes) are ordinary writes to Catalog keys, so they commit or abort with the rest of their Transaction (ADR 0006). Changing a schema or the Indexes rewrites the Collection's entry. A concurrent writer recorded a read of that entry, so it aborts at commit.

**An Index build** (`Node::create_index`) takes the commit lock one chunk at a time (ADR 0013):

1. An ordinary Transaction adds the Index to the Collection's entry as Building, taking its ID from `nextIndexId`. From then on `write_document` maintains it like any Index, but `index::plan` uses only Ready ones. A writer whose Snapshot predates this commit read the old entry, so it aborts with Collection Changed.
2. Each `IndexBuild::step` takes the commit lock and re-reads the Collection's entry at the newest Commit Timestamp. It stops with Index Build Cancelled unless the Namespace, Collection and Index IDs still match and the Index is still Building. It then reads the next chunk (at most 1024 Documents or 16 MiB) at that timestamp and commits its entries through `commit_under_lock`, outside the Write Set cap. A unique Index fails with Duplicate Value only on an entry naming another Document, since a writer may have indexed this one already.
3. After the last chunk, the same step re-reads the entry, marks only this Index Ready and commits. On any failure but cancellation it instead removes only this Index from the newest entry.
4. Between steps, `create_index` yields, so waiting writers get the lock.

On open, Recovery drops every Index still Building in one commit (`drop_building_indexes`).

`Transaction::create_index_on_empty` only works on an empty Collection, where there is nothing to build (ADR 0010).

**Garbage Collection** (`Node::collect_garbage`, in `gc`) takes the Horizon once, then scans every versioned key from the Namespaces up, skipping the unversioned state below them. For each logical key it keeps the newest Version at or below the Horizon (or deletes it too if it is a Tombstone) and deletes every older one, 1024 physical keys per batch, resuming each scan just past the last key it visited. It takes no commit lock and writes nothing to the WAL (ADR 0012), so Recovery may bring removed Versions back for the next pass. Versions newer than the Horizon are never touched, so a concurrent commit is unaffected. Last, it purges the Catalog cache's Tombstones at or below the Horizon.

## Invariants

- **Versions are never changed in place.** A write adds a Version, and a delete adds a tombstone. Garbage Collection only deletes Versions no Snapshot at or after the Horizon can read.
- **The engine stays unaware of Versions.** MVCC, Transactions and the Catalog all live above the `StorageEngine` trait (ADR 0001).
- **Commits are serialised and take one path.** One lock covers conflict checking, the WAL append and the engine write, so Commit Timestamps rise by one and the WAL is in Commit Timestamp order. `CommitState` lives in `node`, beside the commit lock; `Transaction::commit_under_lock` is the only caller of `CommitState::apply_commit`, Index build chunks included. `Node::latest_visible_commit_ts` is stored only by `CommitState::apply_commit`, under the commit lock.
- **The Catalog cache is complete and never behind a Visible commit.** It holds every Namespace and Collection entry ever written, Tombstones included, except Tombstones Garbage Collection purged at or below its Horizon, which every Snapshot from then on reads as absent anyway; and `apply_commit` updates it after the engine write and before publishing, under the commit lock. Publishing first would let a Snapshot at the new timestamp read the entry the commit replaced.
- **Snapshots never go below the Horizon.** `latest_visible_commit_ts` only rises, and registration and `Node::horizon` share one lock, so the Horizon only moves forward. Every Snapshot, Index builds included, registers through `register_snapshot`, which reads the latest Visible timestamp under the registry lock.
- **The WAL comes first.** A commit is in the WAL and fsynced before the engine sees it. If the outcome is unknown, nothing more commits until the Node is reopened.
- **A Document and its Index entries change in one Write Set,** Building Indexes included; only an Index build's chunks write entries on their own, under the commit lock. Index entries point to Document IDs, not to Versions, so a lookup always fetches the Document at its Snapshot and rechecks the filter.
- **IDs are never reused.** Namespace and Collection IDs are random, and their ID keys outlive a drop. Index IDs come from a per-Collection counter that only grows and moves when a build begins, so a failed build's entries sit below it (ADR 0006, ADR 0013).
- **Names and paths are checked once.** A `CollectionPath` or `FieldPath` that exists is valid, so the code below never checks them again.

## Tests

- **Conformance suites:** `psdb_storage::conformance` for every engine, `wal::conformance` for every `WalFile`.
- **Unit tests:** in each module, for encodings, planning and rules.
- **Integration tests:** in `crates/psdb-core/tests/`, one file per area:
  - `catalog`
  - `documents`
  - `isolation`: interleaved Transactions
  - `durability`: replay, poisoning
  - `crash`: simulated power loss on `SimDisk`, and killing a child process
  - `indexes`: a property test checking that `find` through an Index equals a full scan
  - `model`: a property test against a plain map
  - `gc`: Garbage Collection keeps what open Snapshots read
  - `threads`: the Snapshot registry and Horizon, and a stress test on real threads

## Keeping this current

Update this file in the same change as a new crate or module, a changed flow, or a changed invariant. Leave details that change often to the doc comments.
