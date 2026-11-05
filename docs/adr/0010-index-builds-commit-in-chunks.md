# Index builds commit their entries in chunks, in the foreground

Creating an Index on a populated Collection writes one entry per Document, too many to hold in one Write Set under its cap. The build runs in the foreground, holding the commit lock, and commits the entries in chunks, each an ordinary fsynced commit with its own Commit Timestamp; a final commit adds the Index to the Collection's Catalog entry. Every entry is in the WAL, so Recovery (and, later, replicas) only copy bytes, at the cost of a WAL that grows by about the size of each Index built.

## Considered Options

- **Build inside an ordinary Transaction**: simple, but a large Collection fails with Transaction Too Large unless the cap is raised, and the whole build sits in memory.
- **Log only the Index definition and rebuild on apply and in Recovery**: one small record, but Recovery and every future replica must re-run the build, and the code that turns values into Index keys becomes part of the on-disk format, since a changed version would rebuild different entries from the ones committed. MySQL replicates `ALTER TABLE` this way, which is why big index builds cause replication lag there.
- **A new WAL record kind for build chunks, sharing one reserved Commit Timestamp**: the same bytes in the WAL, but it changes the WAL format (refusing every v0.2 WAL), and the reserved timestamp would hold back visibility of later commits once concurrency exists.

## Consequences

- Chunk entries are invisible until the final commit: queries find Indexes through the Catalog at their Snapshot, and no Catalog entry names the Index before then. The WAL format is unchanged.
- Each chunk is fsynced before the next is appended, as every commit is. Recovery treats a bad record followed by a valid one as corruption (ADR 0004); a disk may persist unsynced writes out of order, so syncing only at the end could stop the Node from opening after a crash.
- If the build fails (for example with Duplicate Value) or the Node crashes, its committed chunks stay behind as orphans. No Catalog entry ever names their Index ID, and an ID is never reused (ADR 0006): the Collection's Index ID counter only moves at the final commit, so taking an ID also skips any that already hold data. The orphans sit inside the Collection's key range, below its Documents (ADR 0008), so they never show up as Documents. Garbage collection (v0.4) will remove them.
- On a Collection that holds Documents, `Node::create_index` commits on its own and cannot share a Transaction with other writes: a deliberate exception to ADR 0006, where DDL otherwise runs inside Transactions. On an empty Collection, or one created in the same Transaction, there is nothing to build, so `Transaction::create_index_on_empty` is an ordinary Catalog write that shares the Transaction (as in MongoDB 4.4+). Such a Transaction aborts with Collection Changed if a Document is committed into the Collection after its Snapshot.
- Writes after the build log their Index entries in their Write Sets as usual.
- Holding the commit lock costs nothing while the Database Layer is single-threaded, but once concurrency arrives (v0.4) a long build would stop every other commit for its whole duration. v0.4 must replace this with a background build (for example, building from an older Snapshot, then catching up on later writes and committing quickly).
