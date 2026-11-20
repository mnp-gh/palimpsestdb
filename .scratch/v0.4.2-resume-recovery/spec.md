# v0.4.2: Resume Recovery

Status: ready-for-agent

Recovery brings an engine that already holds data level with the WAL by replaying only the WAL Records above its `applied_up_to`. Design: [ADR 0015](../../docs/adr/0015-wal-is-the-authority-engine-may-lag.md). Needs v0.4.1.

## Out of scope

- A persistent engine (v0.4.4); trimming the WAL (v0.4.5).
- Streaming scans (v0.4.3).

## Recovery

- Read `applied_up_to` from the engine (`keys.rs:19,232`). Missing: fine for an empty engine (treated as 0), `Error::Corruption` otherwise. `Error::EngineNotEmpty` is removed, in `open_with` and `with_limits` (`node.rs:171,227`).
- `applied_up_to` past the WAL's last Record: `Error::Corruption`, WAL left untouched.
- Replay Records with Commit Timestamp above `applied_up_to`; skip the rest (`Wal::open` still reads them all).
- `latest_visible_commit_ts` starts at the larger of `applied_up_to` and the last replayed Record.
- Then fill the Catalog cache from the engine (v0.4.1) and drop Building Indexes, as today.
- Check: the per-Collection "last Document commit" timestamp (`CommitState`, "since the Node opened") stays correct when old Records are not replayed.

## Storage Engine contract

- Trait docs (`crates/psdb-storage/src/lib.rs`) gain: a `write_batch` survives a crash whole or not at all, and the batches that survive are a prefix of those written.
- `impl<E: StorageEngine> StorageEngine for Arc<E>`, forwarding every method.

## Simulator

- A test engine wraps `MemoryEngine` and logs every batch. On a crash, the engine is rebuilt from a random prefix of the log, then the Node reopens on it; the model state after Recovery must match.

## Tests

- Reopen the same `Arc<MemoryEngine>`: data survives, only newer Records are replayed, commits after reopen get higher timestamps.
- Engine ahead of the WAL, and non-empty engine without `applied_up_to`, refuse to open.
- Reopen with nothing to replay starts at `applied_up_to`.
