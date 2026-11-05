# Architecture

A map of the code: what lives where, how a request flows through it, and the rules that hold across modules. It describes v0.3: an embedded library, single-threaded in intent, durable through a WAL over an in-memory Storage Engine.

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
  catalog, index, schema, filter,         what keys and values mean
  document, collection_path, field_path
    keys, mvcc                            key encoding; which Version a Snapshot sees
      StorageEngine                       bytes
wal (beside the stack)                    the log every commit goes through first
```

## Where to look

| Question | Module |
|---|---|
| How is a key laid out? | `keys` (ADR 0008) |
| Which Version does a Snapshot see? | `mvcc` |
| How does a commit get checked and logged? | `transaction` (`commit`), `node` (`apply_commit`) |
| What does a Catalog entry hold? | `catalog` (`CollectionEntry`) |
| Which Index or `_id` range does `find` use? | `index` (`plan`) |
| What is a valid name, `_id` or Document? | `document`, `collection_path`, `field_path` |
| How does recovery work? | `node` (`open_with`), `wal` |

## Keys

Every stored thing is a **logical key** with Versions. A **physical key** is the logical key followed by the inverted Commit Timestamp, so a key's newest Version sorts first (ADR 0008).

- A Document: `namespace_id ‖ collection_id ‖ document_id`.
- An Index entry: `namespace_id ‖ collection_id ‖ 0x00 ‖ index_id ‖ value ‖ document_id`, below every Document of its Collection. A unique Index leaves out the `document_id` (ADR 0011).
- The Catalog: Collections 1 to 4 of Namespace 0 (ADR 0006). Its Collection 0 holds unversioned state: the `applied_up_to` marker.

So everything a Collection owns sits under one prefix, `namespace_id ‖ collection_id`.

## Flows

**Opening.** `Node::open` opens the WAL file. `Wal::open` reads every record and truncates a torn one at the end. `Node::open_with` writes each record into the empty engine with `mvcc::commit_batch`, and the last record's Commit Timestamp becomes the next Snapshot. `Node::new` skips all of this and keeps nothing durable.

**A write** (`Transaction::insert`, `replace`, `delete`) touches only the Transaction's private Write Set:

1. `collection_entry` reads the Collection's Catalog entry, which holds its schema and Indexes, and records the read as a dependency.
2. `document::prepare` checks the limits and the `_id`, generating a UUIDv7 if there is none.
3. `write_document` checks Strict Fields. Then, for every Index, `CollectionEntry::index_entry` works out the old and new entries, and a unique Index refuses a value another Document owns.
4. `write_all` adds the Document and its Index entry changes to the Write Set together, or adds none of them if they would cross the Write Set cap.

**A read** (`Transaction::read`, `scan_range`) sees the Snapshot with the Transaction's own writes laid over it. `find` parses the filter and asks `index::plan` for candidate ranges. With no plan, it scans the Collection's Documents. A `Documents` range is read directly. An `Index` range yields Document IDs, which are then fetched. Every candidate is rechecked against the whole filter (`Filter::matches_raw`).

**A commit** (`Transaction::commit`) takes the commit lock (`Node::commits`), then:

1. Fails if the Node is poisoned. A Transaction that wrote nothing commits at once.
2. Aborts with Collection Changed if a Catalog entry it read, or a Collection it read whole, changed since its Snapshot.
3. Aborts with Write Conflict if any key it writes was committed since its Snapshot (first-committer-wins).
4. Calls `Node::apply_commit`, which takes the next Commit Timestamp, appends and fsyncs the WAL record, then writes one engine batch: every Version plus `applied_up_to`. A failure after the append poisons the Node with Commit Outcome Unknown (ADR 0004).

**Catalog changes** (creating or dropping Namespaces and Collections, schema and Index changes) are ordinary writes to Catalog keys, so they commit or abort with the rest of their Transaction (ADR 0006). Changing a schema or the Indexes rewrites the Collection's entry. A concurrent writer recorded a read of that entry, so it aborts at commit.

**An Index build** (`Node::create_index`) holds the commit lock throughout:

1. `prepare_index` gives the Index the next unused ID.
2. `build_index` reads the Collection at one Snapshot, in chunks of at most 1024 Documents or 16 MiB, and commits each chunk's entries through `apply_commit` directly, outside any Write Set and its cap.
3. A final commit adds the Index to the Collection's entry, which makes the entries reachable.

`Transaction::create_index` only works on an empty Collection, where there is nothing to build (ADR 0010).

## Invariants

- **Versions are never changed in place.** A write adds a Version, and a delete adds a tombstone. Nothing is garbage-collected yet (v0.4).
- **The engine stays unaware of Versions.** MVCC, Transactions and the Catalog all live above the `StorageEngine` trait (ADR 0001).
- **Commits are serialised.** One lock covers conflict checking, the WAL append and the engine write, so Commit Timestamps rise by one and the WAL is in Commit Timestamp order.
- **The WAL comes first.** A commit is in the WAL and fsynced before the engine sees it. If the outcome is unknown, nothing more commits until the Node is reopened.
- **A Document and its Index entries change in one Write Set.** Index entries point to Document IDs, not to Versions, so a lookup always fetches the Document at its Snapshot and rechecks the filter.
- **IDs are never reused.** Namespace and Collection IDs are random, and their ID keys outlive a drop. Index IDs come from a per-Collection counter that only grows (ADR 0006).
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

## Keeping this current

Update this file in the same change as a new crate or module, a changed flow, or a changed invariant. Leave details that change often to the doc comments.
