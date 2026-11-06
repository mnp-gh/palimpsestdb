# In-RAM Catalog cache

Status: ready-for-agent
Blocked by: 01

Spec: [Catalog cache](../spec.md#catalog-cache).

- Newest Version of each Catalog entry with its Commit Timestamp, filled on open and updated under the commit lock.
- Snapshots at or after an entry's timestamp read the cache; older ones read the engine.
