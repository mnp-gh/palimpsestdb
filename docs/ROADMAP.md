# Roadmap

PalimpsestDB grows in small versions, each adding one hard idea on top of a working system. Terms follow [GLOSSARY.md](../GLOSSARY.md); architectural decisions live in [adr/](./adr/).

| Version | Theme | Status |
|---|---|---|
| v0.1 | Embedded, in-memory | In review |
| v0.2 | Durability | Planned |
| v0.3 | Indexes | Planned |
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
- Databases and Collections in a transactional Catalog ([ADR 0006](./adr/0006-transactional-catalog.md)), with numeric IDs in the key layout ([ADR 0005](./adr/0005-shared-engine-numeric-ids.md))
- Order-preserving key encoding, newest Version first
- MVCC with Snapshot Isolation, first-committer-wins ([ADR 0001](./adr/0001-mvcc-lives-above-storage-engine.md))
- Get, insert, replace and delete by Document ID; full-Collection scan with filters (equality, comparison, `$and`, `$or`, dotted paths)
- Autocommit for single operations
- Size limits: Document 16 MB, Document ID 1 KB, nesting depth 100, Database and Collection names 64 bytes without `.` or NUL. Configurable, except limits fixed by the on-disk format
- Property tests with `proptest`

## v0.2: Durability

- WAL owned by the Document Layer, one fsync per commit ([ADR 0004](./adr/0004-document-layer-owns-wal.md))
- Length and CRC32C per record; recovery replays above `applied_up_to`
- Crash test harness: kill at random points, and simulate power loss through a WAL file seam that drops unsynced bytes. After recovery, every acknowledged commit is present, every recovered commit was attempted, and recovered commits form an unbroken run in Commit Timestamp order
- The in-memory engine plus the WAL is a durable database; `psdb-cli --data-dir <path>` opens one

## v0.3: Indexes

- Single-field secondary Indexes, versioned and maintained in the same Write Set
- Index-backed filters
- Evaluate filters on raw BSON (`RawDocument`) instead of decoding whole Documents
- Configurable Write Set cap (default 16 MB)

## v0.4: Concurrency

- Thread-safe Document Layer; single in-order applier; visibility watermark
- In-RAM Catalog cache (v0.1 reads Catalog entries from the engine on every operation)
- Garbage collection of old Versions below the oldest active Snapshot
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

- Users and built-in Roles per Database (`read`, `readWrite`, `dbAdmin`, Node-wide `root`)
- Roles are granted on a Database's ID, not its name: a Database must exist before access to it is granted, and dropping it removes its grants in the same Transaction, so a recreated Database with the same name starts with no access (unlike MongoDB). Record as an ADR when v0.7 starts
- SCRAM-SHA-256 authentication over TLS (`rustls`)

## v0.8: Richer queries

- Partial updates (`$set`, `$inc`), projections, sort
- Compound and multikey Indexes
- Rule-based query planner: equality Index, else range Index, else full scan
- Optional per-Collection validation schemas (Collections stay schemaless by default); the planner uses their type, required and never-an-array facts as hints

## v0.9: Own Storage Engine

- LSM tree (memtable, sorted files, compaction) passing the same conformance suite

## v1.0: Hardening

- Documentation pass, benchmarks, fuzzing, stabilised on-disk format
- Measure the space field names cost after compression; decide on a per-Collection field-name dictionary from the numbers (Documents stay BSON until then)

## Unscheduled

- Query planner statistics (sampled per-path counts and histograms) and a cost model for choosing between Indexes

## v2.x: Distribution

- Define "strong consistency" precisely first (linearizable single-Document operations vs strictly serializable Transactions)
- Replication via Raft over the WAL
- Later: sharding and distributed transactions
