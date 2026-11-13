# Garbage Collection of old Versions

Status: ready-for-agent
Blocked by: 01, 02

Spec: [Garbage Collection](../spec.md#garbage-collection), [ADR 0012](../../../docs/adr/0012-garbage-collection-is-not-logged.md).

- `Node::collect_garbage()`: batches, no commit lock, not logged; removes a tombstone that is the newest Version at or below the horizon. No GC mutex here: concurrent version-by-version passes are safe; issue 05 adds it for reclaim.
- At the end of a pass, purge each cached `(ts, None)` with `ts <= horizon` under the Catalog cache's write lock (a rewrite of the key has `ts > horizon`, so it is never purged).
- `register_snapshot_at` gets `debug_assert!(ts >= self.horizon())`: callers hold the commit lock or register at the latest visible timestamp.
- Returns `GcStats { versions_removed, tombstones_removed }` (tombstones are counted in both). CLI `gc` prints them on one line.
- Tests: (a) an open old Snapshot still reads its value after an overwrite and GC; (b) a deleted Document's Tombstone and older Versions are gone after GC with no open Transactions; (c) a dropped Collection's Catalog Tombstone is purged from the cache and the name can be recreated; (d) a second pass removes nothing. Randomised interleavings wait for the simulator (06).
- Update `ARCHITECTURE.md` for the changed flows and invariants.
