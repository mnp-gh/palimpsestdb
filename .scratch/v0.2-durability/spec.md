# v0.2: Durability

Status: ready-for-human

The in-memory Storage Engine plus a WAL owned by the Document Layer is a durable database: every commit acknowledged as COMMITTED survives a crash. Terms follow [GLOSSARY.md](../../GLOSSARY.md); the design is [ADR 0004](../../docs/adr/0004-document-layer-owns-wal.md).

## Out of scope

- Group commit and checkpoints (v0.6). Until then the WAL grows without bound and every open replays it from the start.
- WAL segments (with checkpoints).
- Persistent Storage Engines and comparing their `applied_up_to` against the WAL (`fjall`, v0.4).
- General Clock and I/O seams and the deterministic simulator (v0.4). v0.2 adds a seam for the WAL file only.

## WAL records

- One record per committed Transaction, holding its Commit Timestamp and its Write Set as logical keys and values (a deleted key is a tombstone). Replay rebuilds the same `WriteBatch` the commit applied.
- Aborted and read-only Transactions write nothing.
- The Commit Timestamp identifies the record; there is no separate LSN. Commit Timestamps are a counter assigned under the commit lock, so no two commits share one.
- Each record carries its length and a CRC32C.

## WAL file

- A single file, `wal`, in the directory given to `open`.
- It starts with a header: an 8-byte magic and a u32 format version.
- Creating it writes the header, fsyncs the file, then fsyncs the directory, so the file itself survives power loss.

## Commit path

Conflict check → assign Commit Timestamp → append the WAL record → fsync (`File::sync_all`, which is `F_FULLFSYNC` on macOS) (Durable) → apply the Write Set plus the `applied_up_to` marker to the engine as one batch, without fsync (Applied) → reply COMMITTED.

The `applied_up_to` marker is written from v0.2, although nothing reads it back from the in-memory engine after a restart.

## Failure handling

- A failed WAL write or fsync poisons the `Database`: every later operation returns an error until it is reopened. Never retry fsync: after a failure the kernel may already have dropped the dirty pages and would report success for data that never reached disk ("fsyncgate").
- The failing commit returns a new error, Commit Outcome Unknown. It is neither an Abort nor retryable, because the Transaction may or may not be present after a restart.

## Opening and recovery

- `Database::open(engine, dir)` opens or creates the WAL and replays it into `engine`. `Database::new(engine)` stays as it is and means "no WAL".
- `open` returns an error if the engine is not empty, meaning it already holds an `applied_up_to` marker.
- Replay applies each record's Write Set straight to the engine, without conflict checks. It then sets the newest Commit Timestamp to the last replayed one, so new Snapshots see every recovered commit.
- A record whose Commit Timestamp does not strictly increase over the previous one is corruption.
- **Torn tail**: a bad record (short, wrong length, or bad CRC) with nothing valid after it is a write interrupted by a crash. It is truncated and open succeeds.
- **Corruption in the middle**: a bad record followed by valid ones means open refuses with an error, rather than silently dropping acknowledged commits.
- **Header mismatch**: a wrong magic or an unknown version means open refuses, and it never truncates or rewrites the file. A wrong path must not destroy someone else's file.
- **Interrupted creation**: a file shorter than the header whose bytes are a prefix of the expected header came from a crash during creation, before any commit could be acknowledged. Open reinitialises it. It refuses any other short file.

## File seam

The WAL does all its I/O through a small trait (append, sync, and the create, truncate and directory-sync calls it needs). The real implementation uses `std::fs`. The fake implementation is for tests:

- It records which bytes have been synced.
- A simulated power loss drops every unsynced byte, optionally keeping a torn prefix of the last unsynced write.
- It models directory durability: a created file whose directory was never synced disappears on power loss.
- It can inject write and fsync failures.

## Crash test harness

Two modes:

1. **Process kill**: run a workload in a child process, kill it with SIGKILL at a random point, then reopen in the parent.
2. **Simulated power loss**: run a workload over the fake file, crash it at a random point, then reopen.

After recovery, the harness checks three things:

1. Every commit that was acknowledged before the crash is present.
2. Every recovered commit was actually attempted; nothing is invented.
3. The recovered commits form an unbroken run in Commit Timestamp order, with no gaps.

A commit that was in flight at the crash may or may not be recovered.

Separate tests cover each rule under Opening and recovery: torn tail, corruption in the middle, header mismatch, interrupted creation, a non-increasing Commit Timestamp, a non-empty engine, and poisoning after an injected fsync failure.

## CLI

`psdb-cli --data-dir <path>` opens a durable database. Without the flag it stays purely in memory, as today.
