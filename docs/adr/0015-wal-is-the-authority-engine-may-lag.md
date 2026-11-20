# The WAL is the authority; the Storage Engine may lag it

A commit fsyncs only the WAL. The Storage Engine is not synced per commit, so a crash may lose its most recent batches; it promises only that each `write_batch` survives a crash whole or not at all, and that the batches which survive are a prefix of those written (no batch survives while an earlier one is lost). Every commit batch carries `applied_up_to`, so after a crash the engine says exactly which commits it holds, and Recovery replays only the WAL Records above it. An engine ahead of the WAL (`applied_up_to` past the WAL's last Record) means the WAL was replaced or cut by hand, and the Node refuses to open rather than guess.

## Considered Options

- **Fsync the engine on every commit too**: the engine would never lag, but every commit would pay two syncs for data the WAL already holds durably.
- **Replay the whole WAL into an empty engine on every open** (v0.1 to v0.4): no contract needed beyond writes, but opening costs time proportional to the whole history and a persistent engine has to be wiped first.
- **Trust the engine when it is ahead of the WAL**: it would open after a lost WAL, but Transactions only the engine holds were never Durable, and later commits would reuse their timestamps.
- **Mark a Checkpoint with an empty WAL Record instead of a header field**: no format change, but a WAL Record would no longer always be a committed Transaction, and cutting then appending is not atomic, so it needs the same file replace anyway.
- **Accept an empty WAL beside a non-empty engine after a Checkpoint**: no new file operation, but a WAL deleted or replaced by hand would then open silently and lose the Durable commits the engine had not synced.

## Consequences

- The Storage Engine contract gains the two crash clauses; the conformance suite checks them for engines that can simulate a crash.
- The simulator keeps a log of engine batches and, on a crash, rebuilds the engine from a random prefix of it, so Recovery is tested from every point the engine can lag to.
- Garbage Collection batches (ADR 0012) are engine batches like any other: a lost one only means the next pass removes those Versions again.
- A Checkpoint (v0.4.5, explicit `Node::checkpoint()`) holds the commit lock, flushes the engine, then replaces the WAL with a header recording `trimmed_up_to`: written to a new file, synced, renamed over the old one, directory synced. "Ahead of the WAL" then means past both the last Record and `trimmed_up_to`, and an engine behind `trimmed_up_to` lost synced data and is Corruption.
- Only an engine that reports itself durable may be checkpointed; trimming behind the in-memory engine would lose every trimmed commit on restart. In the simulator, a flush raises the shortest prefix a crash may keep.
- Commits wait while a Checkpoint syncs the engine; taking Checkpoints off the commit path, and running them automatically, waits for group commit in v0.6.
