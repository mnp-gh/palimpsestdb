# v0.4.1: Catalog from engine

Status: ready-for-human (implemented on `v0.4.1-catalog-from-engine`)

Refactor with no behaviour change. Recovery fills the Catalog cache from the Storage Engine instead of from replayed WAL Records, so v0.4.2 can replay only part of the WAL. Terms follow [GLOSSARY.md](../../GLOSSARY.md).

## Out of scope

- Replaying only part of the WAL; opening a non-empty engine (v0.4.2).

## Change

- `Node::open_with` stops calling `catalog_cache.record` per replayed Record. After replay, it scans every Catalog entry key in the engine and loads the newest Version of each, with its Commit Timestamp, Tombstones included (`crates/psdb-core/src/node.rs:221-245`).
- The cache stays complete: Tombstones Garbage Collection already removed from the engine are absent from both, as before.
- The live commit path keeps calling `record`.

## Tests

- Existing Recovery and Catalog cache tests pass unchanged; the debug cross-check against the engine covers the loaded cache.
- After reopen: a dropped Collection's Tombstone is still cached, and `drop_building_indexes` still finds Building Indexes.
