# v0.4.4: Persistent engine

Status: ready-for-agent

A `fjall` Storage Engine, so data outlives the process. Needs v0.4.2 (otherwise a reopened engine cannot be opened). Design: [ADR 0015](../../docs/adr/0015-wal-is-the-authority-engine-may-lag.md).

## Out of scope

- WAL trimming (v0.4.5); pull iterators (v0.4.6); zstd.
- Testing power loss (data the OS had not yet written to disk); v1.0 Hardening.

## Engine

- New crate `crates/psdb-storage-fjall`, mirroring `psdb-storage-memory`. It depends on `psdb-storage` only, never on `psdb-core`.
- One partition holds every key; the key encoding already separates Namespaces, Collections and Indexes (ADR 0008).
- LZ4 block compression. Check the feature name in the current `fjall` release.
- `write_batch` is one atomic `fjall` batch written to the journal without sync; `flush` syncs fully. Satisfies the crash clauses: whole batches, surviving batches a prefix.

## Opening a Node

- `Node::open(dir, open_engine)` replaces `Node::open(engine, dir)`. The Database Layer owns the layout: the WAL in `dir/wal`, the engine in `dir/engine`, and it calls `open_engine(&dir/engine)` to get the engine (dependency injection: `psdb-core` never names a concrete engine). An engine can then only be paired with its own WAL, except through `open_with`, which stays as it is for the simulator.
  - `Node::open(dir, |path| FjallEngine::open(path))` in the CLI; `Node::open(dir, |_| Ok(MemoryEngine::new()))` in tests and doc examples.
- The CLI's data-directory mode opens `fjall`; in-memory mode keeps `MemoryEngine`. `main` is the only place that picks a concrete engine.
- `Node::new` doc: use it only with an engine that keeps nothing on disk. A persistent engine written without a WAL would later be ahead of any WAL, and `Node::open` refuses it.

## Conformance

- `run_reopen(make, reopen)` beside `run_all` in `psdb-storage/src/conformance.rs`: data written and flushed survives a reopen. No crash check: the simulator's batch log covers the crash clauses.
- `fjall` runs both suites; `MemoryEngine` runs `run_reopen` through `Arc`.

## Tests

- A Node on `fjall` commits, closes, reopens on the same directory, and reads its data back. (Replaying only newer WAL Records is already tested in `tests/durability.rs`.)
- Process crash, in the `fjall` crate: the test starts a child process (itself, selected by an environment variable) that writes numbered batches without `flush`, kills it with `SIGKILL`, reopens the engine and checks that the surviving batches are whole and a prefix of those written.
