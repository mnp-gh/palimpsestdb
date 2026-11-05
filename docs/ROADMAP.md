# Roadmap

PalimpsestDB grows in small versions, each adding one hard idea on top of a working system. Terms follow [GLOSSARY.md](../GLOSSARY.md); architectural decisions live in [adr/](./adr/).

| Version | Theme | Status |
|---|---|---|
| v0.1 | Embedded, in-memory | Done |
| v0.2 | Durability | Done |
| v0.3 | Indexes | In review |
| v0.4 | Concurrency | Planned |
| v0.5 | Serializable | Planned |
| v0.6 | Server | Planned |
| v0.7 | Access | Planned |
| v0.8 | Richer queries | Planned |
| v0.9 | Own Storage Engine | Planned |
| v1.0 | Hardening | Planned |
| v2.x | Distribution | Planned |

## v0.1: Embedded, in-memory

A single-threaded library with no network, no durability and no garbage collection. A crash loses everything.

- Cargo workspace: `psdb-storage`, `psdb-storage-memory`, `psdb-core`, `psdb-cli`
- `StorageEngine` trait with its contract documented on the trait, plus the conformance suite every engine must pass
- In-memory Storage Engine
- BSON Documents, Document IDs (client-supplied or generated)
- Namespaces and Collections in a transactional Catalog ([ADR 0006](./adr/0006-transactional-catalog.md)), with numeric IDs in the key layout ([ADR 0005](./adr/0005-shared-engine-numeric-ids.md))
- Order-preserving key encoding, newest Version first
- MVCC with Snapshot Isolation, first-committer-wins ([ADR 0001](./adr/0001-mvcc-lives-above-storage-engine.md))
- Get, insert, replace and delete by Document ID; full-Collection scan with filters (equality, comparison, `$and`, `$or`, dotted paths)
- Autocommit for single operations
- Size limits: Document 16 MB, Document ID 1 KB, nesting depth 100, Namespace and Collection names 64 bytes without `.` or NUL. Configurable, except limits fixed by the on-disk format
- Property tests with `proptest`

## v0.2: Durability

- WAL owned by the Database Layer, one fsync per commit ([ADR 0004](./adr/0004-database-layer-owns-wal.md))
- Length and CRC32C per record; recovery replays above `applied_up_to`
- Crash test harness: kill at random points, and simulate power loss through a WAL file seam that drops unsynced bytes. After recovery, every acknowledged commit is present, every recovered commit was attempted, and recovered commits form an unbroken run in Commit Timestamp order
- The in-memory engine plus the WAL is a durable database; `psdb-cli --data-dir <path>` opens one

## v0.3: Indexes

- Single-field secondary Indexes, at most one per field path, versioned and maintained in the same Write Set; each has an ID, counted per Collection and never reused, in its key prefix
- Collection Schema declaring a field's Field Type (`int`, `double`, `string`, `date`, `bool`, `objectId`, `binary`) and whether it is a Strict Field ([ADR 0011](./adr/0011-field-types-live-in-collection-schema.md)). Values of other types are left out of the Index; an `int` or `double` Index converts the other kind of number when building its key (the Document is unchanged)
- Unique Indexes, on Strict Fields only, keyed by value alone so concurrent duplicates become Write Conflicts; missing and null never collide
- Index builds in the foreground, committing their entries in chunks outside the Write Set and its cap, made visible by one final commit ([ADR 0010](./adr/0010-index-builds-commit-in-chunks.md))
- Index-backed filters: an equality or range comparison at the top level or in `$and`, whose value is in the Index's type, or an `$or` whose every branch can use an Index (the union of their ranges); equality first, then `$or`, then range; the full filter is always rechecked
- `_id` filters read a range of the Collection itself, which is sorted by `_id`, instead of scanning it: an `_id` equality beats any other, and `_id` comparisons take part in `$or` unions like Index ones
- Exact comparison between integers and doubles in filters (today large integers lose precision when converted to double)
- Evaluate filters on raw BSON (`RawDocument`) instead of decoding whole Documents
- Configurable Write Set cap (default 64 MB: room for a Document of the largest default size, 16 MB, plus Index entries on its large fields)

## v0.4: Concurrency

- Thread-safe Database Layer; single in-order applier; visibility watermark
- Background Index builds, so a long build no longer stops every other commit ([ADR 0010](./adr/0010-index-builds-commit-in-chunks.md))
- In-RAM Catalog cache (v0.1 reads Catalog entries from the engine on every operation)
- Garbage collection of old Versions below the oldest active Snapshot
- Reclaim dropped Collections, dropped Indexes and orphaned Index build chunks: range-delete an ID's keys once no active Snapshot can see a Catalog entry naming it. Version-by-version collection never removes them, because their newest Versions are still live. This also bounds the commit-time check for strict-field changes and Index creation on empty Collections, which today reads every Version ever written under the Collection while holding the commit lock
- `fjall` Storage Engine, passing the conformance suite, with block compression (LZ4 or zstd) enabled
- Streaming scans: `find` iterates instead of loading the whole Collection into memory
- Clock and I/O seams; deterministic simulator

## v0.5: Serializable

- Serializable Snapshot Isolation with Read Markers, escalating key → range → Collection past a configurable per-Transaction threshold; Read Markers outlive commit until every overlapping Transaction finishes ([ADR 0002](./adr/0002-ssi-not-2pl.md))
- Later: read-only safe-snapshot optimisation
- History checker (Elle-style) running in the simulator

## v0.6: Server

- `tokio` network front end ([ADR 0007](./adr/0007-runtime-model.md)) with its own wire protocol ([ADR 0003](./adr/0003-own-wire-protocol.md))
- Rust client with a retrying `with_transaction` helper; CLI over the network
- Group-commit thread, checkpoints, CPU thread pool

## v0.7: Access

- Users and built-in Roles per Namespace (`read`, `readWrite`, `nsAdmin`, Node-wide `root`)
- Roles are granted on a Namespace's ID, not its name: a Namespace must exist before access to it is granted, and dropping it removes its grants in the same Transaction, so a recreated Namespace with the same name starts with no access (unlike MongoDB). Record as an ADR when v0.7 starts
- SCRAM-SHA-256 authentication over TLS (`rustls`)

## v0.8: Richer queries

- Partial updates (`$set`, `$inc`), projections, sort
- Compound and multikey Indexes
- Rule-based query planner: equality Index, else range Index, else full scan
- Validation in the Collection Schema from v0.3, such as required fields (Collections stay schemaless by default); the planner uses their type, required and never-an-array facts as hints

## v0.9: Own Storage Engine

- LSM tree (memtable, sorted files, compaction) passing the same conformance suite

## v1.0: Hardening

- Documentation pass, benchmarks, fuzzing, stabilised on-disk format
- Measure the space field names cost after compression; decide on a per-Collection field-name dictionary from the numbers (Documents stay BSON until then)

## Unscheduled

- Query planner statistics (sampled per-path counts and histograms) and a cost model for choosing between Indexes
- Index hints: the client names the Index a query should use
- Sparse Indexes, as an opt-in: leave out Documents whose field is null or missing, so an Index on a rarely set field stays small; `{field: null}` then scans the whole Collection. Indexes keep null and missing by default
- A `decimal128` Field Type, with exact decimal comparison against integers and doubles in filters (today Decimal128 compares only for equality)
- Range comparisons on binary values: filters only test binary for equality, so a `binary` Index answers equality only. Decide an order first (MongoDB: length, subtype, bytes; our key encoding: subtype, bytes)

## v2.x: Distribution

- Define "strong consistency" precisely first (linearizable single-Document operations vs strictly serializable Transactions)
- Replication via Raft over the WAL
- Later: sharding and distributed transactions
