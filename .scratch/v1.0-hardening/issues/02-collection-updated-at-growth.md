# `collection_updated_at` grows with every Collection ever written

Status: needs-triage

`CommitState::collection_updated_at` (`crates/psdb-core/src/node.rs`) gets an entry the first time a Document is committed into a Collection, and keeps it until the Node is reopened. Dropping the Collection does not remove its entry. A workload that keeps creating, filling and dropping Collections grows the map without bound: about 8 bytes of key plus a heap allocation and a `u64` per Collection.

Harmless for any normal workload; only matters at millions of Collection drops between reopens. A stale entry is never wrong (any Snapshot that sees a reused Collection ID is newer than the drop), it only costs memory.

Open questions:

- Remove the entry when the drop commits, or when Garbage Collection reclaims the dropped ID (v0.4 issue 05)? Removing at drop is safe only if no live Snapshot can still commit a whole-Collection dependency on it; after the drop its commit should already fail with Collection Changed via the Catalog check (verify).
- Or prune entries older than the oldest live Snapshot (the Horizon) on each GC pass: an entry no Snapshot is older than can never cause an abort.
