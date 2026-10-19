# The Document Layer owns the WAL; Storage Engines need not be durable

Durability is provided by a logical write-ahead log owned by the Document Layer, not by each Storage Engine. This gives one durability implementation for every engine (including the in-memory one), puts group commit under our control, and gives future replication a log to replicate. The cost is a double write and our own recovery code; for a single node on one persistent engine, relying on the engine's own durability would have been simpler.

## Consequences

- Commit order: conflict check → assign Commit Timestamp → append WAL record → fsync (Durable) → apply Write Set to the engine as one atomic batch, including an `applied_up_to` marker, without fsync (Applied) → advance the visibility watermark (Visible) → reply COMMITTED.
- A single applier applies commits to the engine strictly in Commit Timestamp order, so `applied_up_to` and the visibility watermark only ever advance over an unbroken run.
- The engine contract shrinks to: ordered keys, atomic batches applied in order, and `flush()` for checkpoints. Recovery replays WAL records above `applied_up_to`.
- WAL records carry length and CRC32C; replay stops at the first bad record (torn write).
- On macOS a plain `fsync` does not flush the disk cache; WAL durability uses `File::sync_all`, which issues `F_FULLFSYNC` on Apple platforms. Its cost is why group commit exists.
- Staged delivery: no WAL in v0.1, WAL + recovery in v0.2, group commit and checkpoints in v0.3+.
