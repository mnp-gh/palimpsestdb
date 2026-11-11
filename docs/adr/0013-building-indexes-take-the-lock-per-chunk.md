# Building Indexes are maintained by writers; the build takes the commit lock per chunk

From v0.4 an Index build no longer holds the commit lock end to end (ADR 0010). A first commit adds the Index to the Catalog as Building: from then on every write maintains its entries and enforces its uniqueness, but queries do not use it. The build then takes the commit lock for one chunk at a time, reads that chunk's Documents at the newest Commit Timestamp and commits their entries; between chunks other Transactions commit as usual. A final commit marks the Index Ready. Other work pauses for one chunk, not the whole build, and no catch-up pass is needed. Three rules keep the Index right:

- A chunk reads and commits under the commit lock, so no Document can change between reading it and committing its entries.
- A writer whose Snapshot predates the Building commit does not know the Index exists and writes no entries for it; it read the Collection's Catalog entry, which that commit changed, so it aborts with Collection Changed (the existing Catalog dependency check).
- A writer whose Snapshot includes the Building commit writes entries for its old and new Version. If a chunk wrote entries for the same Document after that Snapshot, the writer either changed the Document concurrently (Write Conflict on the Document) or writes an entry key the chunk wrote (Write Conflict on the entry); otherwise its old Version is the one the chunk indexed, so its tombstone removes exactly that entry. A chunk that reaches a Document after a writer committed reads the new Version and writes the same entries again.

## Considered Options

- **Build from an older Snapshot, then catch up** (ADR 0010's suggestion): the build holds no lock while scanning, but catching up needs a scan of every Document changed since that Snapshot, repeated until few remain, then a final pass under the lock.
- **Keep the foreground build**: simplest, but every other commit and every new Transaction would wait for the whole build once the Database Layer is thread-safe.

## Consequences

- A write can fail with Duplicate Value against an Index that queries do not use yet (as in MongoDB).
- Builds cause extra retryable Aborts: Collection Changed for Transactions open across the Building or Ready commit, and Write Conflicts with chunks on entry keys.
- The Catalog entry of an Index records whether it is Building or Ready.
- The Building commit takes the Index ID and moves `nextIndexId` past it, not the final commit as in ADR 0010, so Garbage Collection can tell a failed build's entries (ID below the counter, named by no Index) from a build that started after its Horizon.
- A Building Index already counts as its field's one Index: a second create on that field fails as if it existed, dropping the Collection or the Index stops the build at its next chunk with Index Build Cancelled, which is not retryable (the only ways to stop one), and a duplicate found by a unique build fails it with Duplicate Value and one commit removes its Catalog entry.
- Recovery drops any Index still Building, since no build survives a restart; the client creates it again, and Garbage Collection reclaims its entries.
- Chunks remain ordinary fsynced commits, so the WAL format and ADR 0010's reasons for chunking are unchanged.
