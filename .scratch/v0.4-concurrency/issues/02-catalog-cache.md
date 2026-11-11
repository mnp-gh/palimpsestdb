# In-RAM Catalog cache

Status: ready-for-agent
Blocked by: 01

Spec: [Catalog cache](../spec.md#catalog-cache).

- Newest Version (raw bytes) of each Namespace and Collection entry with its Commit Timestamp, in an `RwLock<HashMap>` on `Node`; filled by WAL replay on open and updated under the commit lock, after the engine write and before publish.
- Point reads in `Transaction::read` only: Snapshots at or after an entry's timestamp read the cache, older ones read the engine, a miss is absent. Listings and ID keys stay on the engine.
- Debug-build cross-check against the engine, plus tests for an old Snapshot after a drop, a missing Namespace, and own write over cache.
- Update `ARCHITECTURE.md` for the changed flows and invariants.
