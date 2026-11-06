# Garbage Collection of old Versions

Status: ready-for-agent
Blocked by: 01

Spec: [Garbage Collection](../spec.md#garbage-collection), [ADR 0012](../../../docs/adr/0012-garbage-collection-is-not-logged.md).

- `Node::collect_garbage()`: batches, no commit lock, not logged; removes a tombstone that is the newest Version at or below the horizon.
- CLI `gc`.
- Test: never removes a Version an open Snapshot reads.
