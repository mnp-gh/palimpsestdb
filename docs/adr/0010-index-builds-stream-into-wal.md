# Index builds stream their entries into the WAL in chunks

Creating an Index on a populated Collection writes one entry per Document, too many to hold in one Write Set under its cap. The build reserves its Commit Timestamp up front, then streams its entries as a run of Index Build Records, each appended to the WAL and applied to the Storage Engine before its memory is freed, and finishes with an ordinary commit at that timestamp that writes the Index's Catalog entry. Recovery and replicas therefore only copy bytes, as for every other write, at the cost of a WAL that grows by about the size of each Index built.

## Considered Options

- **Build inside an ordinary Transaction**: simple, but a large Collection fails with Transaction Too Large unless the cap is raised, and the whole build sits in memory.
- **Log only the Index definition and rebuild on apply and in Recovery**: one small record, but Recovery and every future replica must re-run the build, and the code that turns values into Index keys becomes part of the on-disk format, since a changed version would rebuild different entries from the ones committed.

## Consequences

- On a Collection that holds Documents, `createIndex` commits on its own and cannot share a Transaction with other writes: a deliberate exception to ADR 0006, where DDL otherwise runs inside Transactions. Inside a Transaction, the reserved Commit Timestamp would be held for as long as the client keeps it open, and an Abort would leave its Index Build Records behind in the WAL. On an empty Collection, or one created in the same Transaction, there is nothing to build, so `createIndex` is an ordinary Catalog write that shares the Transaction (as in MongoDB 4.4+).
- Amends ADR 0004: the WAL now holds Index Build Records as well as one WAL Record per committed Transaction. Index Build Records carry the Commit Timestamp of the commit that follows them and do not advance it, so a run of them sharing one timestamp is not corruption.
- Only the final commit is fsynced; because the WAL is sequential, that one fsync makes every earlier Index Build Record durable too.
- The Index exists only once its Catalog entry commits, and queries find Indexes through the Catalog at their Snapshot, so entries from an unfinished build are never read. After a crash mid-build, Recovery replays the Index Build Records it finds; no Catalog entry ever names their Index ID, which is never reused (ADR 0006), so they stay harmless until garbage collection (v0.4) removes them.
- Writes after the build log their Index entries in their Write Sets as usual.
- The build's reserved Commit Timestamp holds back visibility of every later commit until the build commits, because commits become Visible only over an unbroken run (ADR 0004). Harmless while the Document Layer is single-threaded; once concurrency arrives (v0.4), a long build would hide every other commit for its whole duration, so v0.4 must replace this with a build that holds no timestamp while it runs (for example, building from an older Snapshot, then catching up on later writes and committing quickly).
