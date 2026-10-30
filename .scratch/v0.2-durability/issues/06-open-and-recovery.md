# Open and recovery

Status: ready-for-agent

Blocked by: 03, 04, 05

Spec: [../spec.md](../spec.md)

## What

Add `Database::open(dir, engine)` (and a `with_limits` variant). It creates the directory and `wal.log` if missing, then runs Recovery:

1. read `applied_up_to` from the engine (absent means 0);
2. read the WAL; truncate the file at the end of the valid prefix;
3. replay every record with a Commit Timestamp above `applied_up_to`, in order, writing Versions with `physical_key` plus the marker, exactly as the commit path does;
4. set `last_committed` to the highest Commit Timestamp seen (today it always starts at 0, `database.rs`).

Share the "Write Set to batch" code between commit and replay so the two cannot drift.

## Tests

- Commit some Documents and DDL, drop the `Database`, reopen over a fresh `MemoryEngine` and the same WAL: every Document, Database and Collection is back, and the next commit gets the next timestamp.
- A torn last record is truncated and its commit is absent.
- A record at or below `applied_up_to` is not reapplied.

## Done when

Reopening a WAL-backed `Database` restores every acknowledged commit.
