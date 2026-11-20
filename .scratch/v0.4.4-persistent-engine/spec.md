# v0.4.4: Persistent engine

Status: ready-for-agent

A `fjall` Storage Engine, so data outlives the process. Needs v0.4.2 (otherwise a reopened engine cannot be opened). Design: [ADR 0015](../../docs/adr/0015-wal-is-the-authority-engine-may-lag.md).

## Out of scope

- WAL trimming (v0.4.5); zstd.

## Engine

- New crate `crates/psdb-storage-fjall`, mirroring `psdb-storage-memory`.
- One partition holds every key; the key encoding already separates Namespaces, Collections and Indexes (ADR 0008).
- LZ4 block compression. Check the feature name in the current `fjall` release.
- `write_batch` is one atomic `fjall` batch written to the journal without sync; `flush` syncs fully. Satisfies the crash clauses: whole batches, surviving batches a prefix.

## Conformance

- `run_reopen(make, reopen)` beside `run_all` in `psdb-storage/src/conformance.rs`: data written and flushed survives a reopen; optionally, batches after the last flush survive whole and as a prefix when the engine can simulate a crash.
- `fjall` runs both suites; `MemoryEngine` runs `run_reopen` through `Arc` without the crash check.

## Tests

- A Node on `fjall` commits, closes, reopens on the same directory, and reads its data back; Recovery replays only newer WAL Records.
