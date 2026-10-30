# v0.2: Durability

Status: ready-for-agent

## Goal

A `Database` opened over a directory survives crashes: every commit acknowledged before a crash is present after reopening, and no unacknowledged commit is partly visible. The engine is still the in-memory one; the WAL makes it durable ([ADR 0004](../../docs/adr/0004-document-layer-owns-wal.md)).

## Commit path

conflict check → assign Commit Timestamp → append WAL Record → `sync_all` (Durable) → `write_batch` of the Versions plus the `applied_up_to` marker, no fsync (Applied) → reply.

v0.1 is single-threaded, so Applied and Visible are the same step; the separate visibility watermark arrives in v0.4.

## Decisions

- **D1 Crate.** The WAL lives in a new crate, `psdb-wal`. `psdb-core` depends on it.
- **D2 Record format.** The file starts with `"PWAL" ‖ format version u32` (version 1). Each Record is `[len u32][crc32c u32][payload]`, payload `[commit_ts u64][entry count u32]` then per entry `[key len u32][logical key][tag u8]` and, for a put, `[value len u32][value]`. All integers big-endian, written with `to_be_bytes`, never native endian. `len` is the payload length; the CRC32C covers the payload. Records hold logical keys, not physical keys, so the WAL does not depend on the Version key layout (ADR 0008). Recorded in ADR 0010.
- **D3 One file.** A single `wal.log` in the data directory, its name produced by one function so segments can replace it later.
- **D4 `applied_up_to` key.** A reserved, unversioned key in Database 0, Collection 0 (no timestamp suffix). Its value is the big-endian Commit Timestamp of the last applied commit. Added to ADR 0008.
- **D5 Failure.** If appending, syncing or applying fails during commit, the `Database` is marked failed and that and every later operation return `Error::CommitOutcomeUnknown`. A failed fsync is never retried. Reopening runs recovery.
- **D6 Fault injection.** The WAL runs over a `LogFile` trait (append, `sync_all`, read all, truncate). Tests use a fake that keeps only synced bytes on a simulated crash, optionally tearing the last write. One test kills a real process with `SIGKILL`.
- **D7 Retention.** The WAL grows without bound in v0.2: the in-memory engine's `flush()` is a no-op, so no record can ever be truncated. Documented as a known limitation.
- **D8 Checkpoints.** Arrive with the first durable engine (v0.4). ADR 0004 and the roadmap are updated to say so.

## Recovery

On open: read `applied_up_to` from the engine (absent means 0), read the WAL, stop at the first record with a short length or bad CRC and truncate the file there, replay every record above `applied_up_to` in order through `physical_key`, and set `last_committed` to the highest Commit Timestamp seen.

## Out of scope

Group commit, checkpoints, WAL segments, a durable Storage Engine, garbage collection, thread safety, and an I/O seam beyond the WAL file.

## Tickets

1. [Docs housekeeping](issues/01-docs-housekeeping.md)
2. [ADR 0010: WAL record format and commit failure](issues/02-adr-wal-format.md)
3. [`psdb-wal` crate](issues/03-psdb-wal-crate.md)
4. [`applied_up_to` key](issues/04-applied-up-to-key.md)
5. [Durable commit path](issues/05-durable-commit.md)
6. [Open and recovery](issues/06-open-and-recovery.md)
7. [Crash test harness](issues/07-crash-tests.md)
8. [CLI data directory](issues/08-cli-data-dir.md)
9. [Release v0.2.0](issues/09-release.md)
