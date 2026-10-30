# Durable commit path

Status: ready-for-agent

Blocked by: 03, 04

Spec: [../spec.md](../spec.md)

## What

Change `Transaction::commit` (`crates/psdb-core/src/transaction.rs`) so that, for a `Database` with a WAL, after the conflict check and timestamp assignment it:

1. builds a WAL Record from the Write Set (logical keys, puts and tombstones) and the Commit Timestamp;
2. appends it and syncs (Durable);
3. calls `write_batch` with the Versions plus the `applied_up_to` key set to the Commit Timestamp (Applied);
4. advances `last_committed` and returns.

All of this runs under the existing `last_committed` lock. If step 2 or 3 fails, mark the `Database` failed (D5) and return `Error::CommitOutcomeUnknown`; every later operation on a failed `Database` returns the same error. A read-only Transaction writes nothing to the WAL.

The `Database` gains an optional WAL; `Database::new` keeps working without one.

## Tests

- A commit writes exactly one WAL Record with the right timestamp and entries.
- A failed sync or apply (via `FaultyLogFile`) returns `CommitOutcomeUnknown`, and later operations fail the same way.

## Done when

Commits on a WAL-backed `Database` follow the spec's commit path and all v0.1 tests still pass.
