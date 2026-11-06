# Reclaim dropped Collections, dropped Indexes and orphaned build chunks

Status: ready-for-agent
Blocked by: 03

Spec: [Garbage Collection](../spec.md#garbage-collection).

- Part of `collect_garbage()`: once the horizon passes the drop, scan the ID's key range and delete in batches. No range-delete operation in the Storage Engine.
- Orphans: Index IDs holding data that no Catalog entry names.
