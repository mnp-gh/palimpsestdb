# Garbage Collection deletes from the Storage Engine without a WAL Record

Garbage Collection removes Versions that no active Snapshot can read, by deleting them directly in the Storage Engine; it writes nothing to the WAL. Each Transaction registers its Snapshot when it begins and removes it when it ends, and the oldest registered Snapshot bounds what may be removed. Leaving collection out of the WAL keeps the WAL a record of commits only and costs nothing at commit time; the price is that a collected Version can come back after Recovery, which is harmless because the next pass removes it again.

## Considered Options

- **Log each pass as a WAL Record**: Recovery and future replicas would see exactly the same physical Versions, but every pass grows the WAL, and a pass would need a Commit Timestamp although it changes nothing any Snapshot can see.
- **Let the Storage Engine collect (compaction filters)**: efficient on an LSM, but the engine would have to understand Versions and Snapshots, which ADR 0001 keeps above it.

## Consequences

- Recovery may bring back Versions a pass already removed (today it replays the whole WAL into an empty engine, so all of them come back). Collection is idempotent, so the next pass removes them again.
- Replicas (v2.x) collect on their own, against their own oldest Snapshot; nothing about collection travels over replication.
- A Version is never removed while an active Snapshot could read it, so collection never changes what a Transaction sees.
