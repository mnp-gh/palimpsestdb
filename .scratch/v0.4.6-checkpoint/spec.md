# v0.4.6: Checkpoint

Status: ready-for-agent

An explicit Checkpoint cuts WAL Records the Storage Engine no longer needs, so the WAL stops growing without bound and opens read less. Needs v0.4.5. Design: [ADR 0015](../../docs/adr/0015-wal-is-the-authority-engine-may-lag.md); staged delivery in [ADR 0004](../../docs/adr/0004-database-layer-owns-wal.md).

## Out of scope

- Running Checkpoints automatically, or off the commit path (v0.6, with group commit).
- Segmented WAL files.

## Checkpoint

- `Node::checkpoint()` takes the commit lock, calls the engine's `flush`, then replaces the WAL with a header whose `trimmed_up_to` is the newest Applied commit. Every Durable commit is Applied under the commit lock, so the whole WAL goes.
- Returns an error when the engine's `is_durable()` is false.
- A poisoned Node (a failed fsync left a commit's outcome unknown) returns the same error commits do and leaves the WAL untouched: a `trimmed_up_to` must never cover a commit that may not be Durable.
- CLI: a `checkpoint` command beside `gc`, printing the error in in-memory mode.
- A crash between flush and replace leaves the old WAL; Recovery skips its Records.

## WAL

- Header gains `trimmed_up_to`; format version bumps, no compatibility with old files.
- `WalFile::replace(bytes)`: write a new file, sync it, rename it over the WAL, sync the directory. `OsWalFile` and `SimWalFile` implement it; the simulated disk can crash before or after the rename.
- Recovery: "ahead of the WAL" means past both the last Record and `trimmed_up_to`. An engine whose `applied_up_to` is below `trimmed_up_to` lost synced data: `Error::Corruption`. The first Record after a cut must be above `trimmed_up_to`.

## Storage Engine

- `fn is_durable(&self) -> bool`, default `false`. `MemoryEngine` false, `Arc<E>` forwards, `fjall` true, the simulator's engine true.
- In the simulator, `flush` raises the shortest prefix a crash may keep to every batch logged so far.

## Tests

- Checkpoint then reopen: data intact, WAL holds only a header, commits continue above `trimmed_up_to`.
- Checkpoint on `MemoryEngine` errors and leaves the WAL untouched.
- Checkpoint on a poisoned Node errors and leaves the WAL untouched (beside `failed_fsync_poisons_and_leaves_the_outcome_unknown`).
- Simulator interleaves Checkpoints with Transactions, GC steps and crashes.
