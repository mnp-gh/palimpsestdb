# Repo audit, 2026-11-22 (at v0.4.2, commit 533c2cb)

Whole-repo review as a senior Rust engineer, plus a comparison with established databases. `cargo test --workspace --release` passed.

Load assumed: one process, a few threads, data that fits in RAM.

## Findings

New, not on the roadmap (filed as issues):

1. Plain reads wait behind writes that are saving to disk: [issues/01-read-only-commit-skips-lock.md](issues/01-read-only-commit-skips-lock.md)
2. Two live Nodes on one shared engine silently corrupt each other: [issues/02-shared-engine-two-nodes.md](issues/02-shared-engine-two-nodes.md)

Already planned, noted for priority:

3. **A full scan blocks every commit and copies the whole collection** (`transaction.rs:193`, `psdb-storage-memory/src/lib.rs:33`). `MemoryEngine::scan` holds its read lock for the whole scan, and `scan_snapshot` copies every live Document into a `BTreeMap` before `find` returns a `Vec`. A 2 GB Collection means about 2 GB extra RAM, and every commit stalls on the engine's write lock until the scan ends. Planned as v0.4.3 (Streaming find); do it before v0.4.4.
4. **The WAL never shrinks; every restart replays and loads all of it** (`wal/mod.rs:87`). `Wal::open` reads the whole file into memory, and recovery over the in-memory engine replays every commit ever made, including Versions GC already removed. Opening needs about (WAL size + data size) of RAM. Planned as v0.4.4/v0.4.5.
5. **GC is manual only.** Old Versions pile up until `collect_garbage()` is called, and a long-open Transaction holds back cleanup with no timeout. Auto-GC is deferred on the roadmap.

## Compared with real databases

| Area | PalimpsestDB today | SQLite / Postgres / RocksDB / MongoDB / Firestore |
|---|---|---|
| Data size | Must fit in RAM (BTreeMap) | On-disk B-trees or LSM trees with a page cache |
| Restart time | Replays the entire WAL history | Checkpoints keep recovery short |
| Write throughput | One fsync per commit under one global lock (≈100–1000 commits/s) | Group commit: many commits share one fsync |
| Isolation | Snapshot Isolation, so write skew can happen | Postgres offers SSI; SQLite and Firestore are serializable |
| Old-version cleanup | Manual GC, not logged, so a restart brings removed Versions back | Autovacuum, compaction, run automatically |
| Updates | Whole-Document replace; every Version stores the full Document, uncompressed | Partial updates (`$set`, `$inc`), compression |
| Queries | Filters only: no sort, limit, projection or cursors; single-field Indexes | Planners, compound/multikey Indexes, streaming cursors |
| Access | One process, embedded, no network, no auth | Server, auth, TLS (SQLite is also single-process) |
| Operations | No backup, replication, metrics or online schema tools | Built in |

Better than most hobby databases: crash tests over a simulated disk and a deterministic simulator; poisoning instead of retrying after a failed fsync (the Postgres "fsyncgate" lesson); `F_FULLFSYNC` on macOS; prefix-free, order-preserving keys checked with property tests; CRC32C on WAL Records with mid-log damage refused; `unsafe` forbidden; ADRs.

## Verdict

Healthy for a learning project. Fix 01 and 02 first (small, not on the roadmap), then do v0.4.3 before v0.4.4.

Not checked: `catalog.rs`, most of `index.rs`, the CLI, filter planning, test bodies. Index-build race handling taken on trust from ADR 0013 and the simulator tests. No benchmarks run.
