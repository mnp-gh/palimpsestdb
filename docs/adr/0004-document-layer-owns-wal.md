# The Document Layer owns the WAL; Storage Engines need not be durable

Durability is provided by a logical write-ahead log owned by the Document Layer, not by each Storage Engine. This gives one durability implementation for every engine (including the in-memory one), puts group commit under our control, and gives future replication a log to replicate. The cost is a double write and our own recovery code; for a single node on one persistent engine, relying on the engine's own durability would have been simpler.

## Consequences

- Commit order: conflict check → assign Commit Timestamp → append WAL record → fsync (Durable) → apply Write Set to the engine as one atomic batch, including an `applied_up_to` marker, without fsync (Applied) → advance the visibility watermark (Visible) → reply COMMITTED.
- A single applier applies commits to the engine strictly in Commit Timestamp order, so `applied_up_to` and the visibility watermark only ever advance over an unbroken run.
- The engine contract shrinks to: ordered keys, atomic batches applied in order, and `flush()` for checkpoints. Recovery replays WAL records above `applied_up_to`.
- One WAL record per committed Transaction, holding its Commit Timestamp and Write Set; aborted and read-only Transactions write nothing. The Commit Timestamp identifies the record, so there is no separate LSN, and replay treats a non-increasing timestamp as corruption.
- WAL records carry length and CRC32C. A bad record at the tail is a torn write from a crash and is truncated; a bad record followed by valid ones is corruption, and open refuses rather than silently dropping acknowledged commits.
- A failed WAL write or fsync poisons the Database until it is reopened, and the failing commit returns Commit Outcome Unknown. Retrying fsync is unsafe: the kernel may already have dropped the dirty pages and would report success for data that never reached disk (PostgreSQL's 2018 "fsyncgate").
- The WAL file starts with a magic number and a format version. Open refuses a file whose magic or version does not match and never truncates it, so a wrong path cannot destroy someone else's file. A file shorter than the header whose bytes are a prefix of the expected header is a crash during creation, before any commit was acknowledged, and is reinitialised.
- Creating the WAL fsyncs the file and then its directory, so the file itself survives power loss.
- On macOS a plain `fsync` does not flush the disk cache; WAL durability uses `File::sync_all`, which issues `F_FULLFSYNC` on Apple platforms. Its cost is why group commit exists.
- Staged delivery: no WAL in v0.1, WAL + recovery in v0.2, group commit and checkpoints in v0.6. Until checkpoints exist, the WAL grows without bound and every open replays it from the start.
