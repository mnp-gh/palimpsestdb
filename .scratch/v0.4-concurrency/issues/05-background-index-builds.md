# Background Index builds (Building → Ready)

Status: ready-for-agent
Blocked by: 01

Spec: [Index builds](../spec.md#index-builds), [ADR 0013](../../../docs/adr/0013-building-indexes-take-the-lock-per-chunk.md).

- `state` in the Catalog's `indexes` entries; old entries read as Ready.
- Writers maintain Building Indexes; the planner uses only Ready ones.
- The commit lock is taken per chunk, reading at the newest Commit Timestamp.
- Drop cancels the build; Recovery drops Building Indexes.
- CLI `indexes` shows the state.
