# Index Catalog entries

Status: resolved
Blocked by: 02, 03

Spec: [Index Catalog entries](../spec.md#index-catalog-entries), [ADR 0014](../../../docs/adr/0014-indexes-have-their-own-catalog-entries.md).

- New Catalog Collection 5 in `keys.rs`: `namespace_id ‖ collection_id ‖ index_id (u32 BE)`, value `{ path, unique, state }`. The Collection entry drops `indexes` and keeps `nextIndexId`. No support for entries with `indexes`.
- Create: write the Index entry and rewrite the Collection entry (`nextIndexId`). Drop: Tombstone on the Index entry only. Ready: rewrite the Index entry only, so `mark_ready` and the failed-build cleanup no longer read-modify-write the Collection entry. Recovery's `drop_building_indexes` and the cleanup write Tombstones.
- A build chunk checks that its Index entry still exists and is Building, plus the Collection ID, instead of scanning `indexes`.
- Writers record a read of the Collection entry and of every Index entry they maintain (`catalog_entry_reads`), so create, drop and Ready abort them with Collection Changed.
- Catalog cache: `HashMap` → `BTreeMap`, holding Index entries too; a Collection's Indexes are a prefix range of it (Snapshots older than an entry fall back to the engine, as for point reads).
- Update the `catalog.rs` module comment and `ARCHITECTURE.md`.
- Tests: (a) creating and dropping an Index aborts a concurrent writer with Collection Changed, and so does marking it Ready; (b) marking one Index Ready leaves a concurrent build on another field intact; (c) after Recovery, a Building Index's entry is a Tombstone; (d) an old Snapshot still sees an Index dropped after it began; (e) the debug cross-check covers Index entries.
