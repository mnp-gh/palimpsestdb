# Reclaim dropped Namespaces, Collections and Indexes

Status: ready-for-agent
Blocked by: 03, 04

Spec: [Garbage Collection](../spec.md#garbage-collection).

- Part of `collect_garbage()`, in its one scan: the Catalog sorts first, so dead IDs are known before any data. A Namespace or Collection is dead when the pass deletes its entry's Tombstone (the Version before it holds the ID); a dead Namespace's Collections are dead and its Collection entries are deleted from the engine and the Catalog cache. An Index is dead when the newest Collection entry at the Horizon lists it as `dropped`.
- Every key under a dead prefix is deleted in batches, whatever its timestamp. No range-delete operation in the Storage Engine.
- Catalog deletes are written after the data, so a crash never loses the Tombstone marking data dead. ID keys and `nextIndexId` are never deleted.
- Dropping an Index (client, failed build, Recovery's `drop_building_indexes`) marks it `state: "dropped"` in `indexes` instead of removing it; every reader of `indexes` skips it. The marker stays (`shortcut:` comment, issue 08). Update the `catalog.rs` module comment for the new state.
- No orphaned build chunks: data written before ADR 0013 is not supported.
- Add the GC mutex (moved here from 04): one pass at a time, never blocking commits.
- `GcStats` adds `collections_reclaimed` and `indexes_reclaimed`, counted only when the pass deleted something under the ID; reclaimed keys also count in `versions_removed` and `tombstones_removed`. CLI `gc` prints all four.
- Commit-time check for strict-field changes and Index creation on empty Collections (`transaction.rs`): comment only, no code change (issue 07).
- Tests: (a) a dropped Collection's Documents and Index entries are gone after GC; (b) a dropped Namespace's Collections, their data and Collection entries are gone; (c) a dropped Index's entries are gone and another Index on the Collection is untouched; (d) a build failing with Duplicate Value leaves no entries after GC; (e) a Transaction begun before a drop still reads the data, which goes only after it ends; (f) a Collection created after the Horizon is untouched; (g) a second pass reclaims nothing. Randomised interleavings wait for the simulator (06).
- Update `ARCHITECTURE.md` for the changed flows and invariants.
