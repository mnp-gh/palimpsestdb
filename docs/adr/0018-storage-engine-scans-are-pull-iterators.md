# Storage Engine scans are pull iterators with a loose view

Supersedes [ADR 0016](./0016-streaming-reads-rescan-in-chunks.md). From v0.4.4 the Storage Engine's `scan` returns an iterator the caller pulls from, so `find` streams from one engine iterator instead of rescanning in chunks. The engine promises only a loose view: keys in ascending order, and each key not written while the iterator is alive exactly once, with its value; a key written meanwhile has no promise (new value, old value, or not at all), and after an error the iterator ends. That is enough because MVCC lives above the engine (ADR 0001): a Snapshot is only taken at the visibility watermark, so every Version it can read is in the engine before its scan opens; a commit only adds Versions newer than every registered Snapshot, which reads skip; and Garbage Collection only removes Versions no registered Snapshot can read.

## Considered Options

- **Keep push-style `scan` and rescan in chunks** (ADR 0016, v0.4.3): no iterator state in engines, but every chunk re-seeks, and the Database Layer carries chunk limits and resume keys.
- **Promise a frozen view** (RocksDB, LevelDB, Pebble, `fjall`): cheap in an LSM, whose files never change, but the in-memory engine would need a persistent map (a new dependency) or sequence-numbered entries, and every future engine would owe it. MVCC above already gives readers their frozen view.
- **Hold the in-memory engine's read lock while the iterator lives**: simplest, but a caller that pauses between Documents would block every writer.

## Consequences

- The in-memory engine copies a few entries at a time under its read lock and re-seeks past the last key returned, as InnoDB's cursor restores its position after releasing a latch.
- An iterator kept open in `fjall` pins its files, as a registered Snapshot already holds back Garbage Collection.
- The argument needs a fixed Snapshot. A read whose timestamp moves, like an Index build reading each chunk at the newest Commit Timestamp, opens a fresh iterator per chunk: one kept open could return a Version that a writer replaced between chunks, stale but still not "written meanwhile" from the build's point of view.
- Only an explicit Transaction streams; `Node::find` collects into a `Vec` because its Transaction ends before it returns (as PostgreSQL's JDBC driver streams only outside autocommit). A cursor that owns its Transaction can come with the v0.6 server.
