# WAL Record format, Recovery, and a failed commit poisons the Database

Each committed Transaction becomes one length-prefixed, checksummed WAL Record holding logical keys, appended to a single `wal.log` and synced with `File::sync_all` before the Transaction is Applied (ADR 0004). Every integer is big-endian, as in keys (ADR 0008), so the project has one byte-order rule. The WAL is part of the on-disk format, so the layout is fixed here.

## Layout

- **Record**: `len (u32 BE) ‖ crc (u32 BE) ‖ payload`. `len` is the payload length; `crc` is CRC32C of the payload.
- **Payload**: `commit_ts (u64 BE) ‖ entry_count (u32 BE) ‖ entries…`.
- **Entry**: `key_len (u32 BE) ‖ logical key ‖ tag (u8)`, then for a put `value_len (u32 BE) ‖ value`. Tag `0x00` is a tombstone, `0x01` a put; the value is the Document's BSON bytes or a Catalog entry.
- **File**: one `wal.log` in the data directory: a header `"PWAL" ‖ format_version (u32 BE)`, currently version 1, then Records back to back. A reader rejects a file with another magic or version, so the format (for example the width of `commit_ts`) can change later by bumping the version and keeping a reader for the old one.

Records hold logical keys, not physical keys: Recovery builds physical keys with the same function the commit path uses, so the WAL does not depend on the Version layout of ADR 0008.

## `applied_up_to`

Each commit's `write_batch` also sets an unversioned key in Database 0, Collection 0 to the Commit Timestamp (u64 BE). Recovery replays only Records above it; absent means 0.

## Consequences

- **Torn header.** A file shorter than the header whose bytes are a prefix of it was cut short while being created; it is reset to an empty WAL.
- **Torn tail.** Recovery reads Records in order and stops at the first one whose header or payload is cut short or whose CRC does not match, then truncates the file there. Such a Record was never synced, so its Transaction was never acknowledged.
- **Commit Outcome Unknown.** If appending, syncing or applying fails during commit, the Transaction may or may not be Durable, so it is neither committed nor aborted. The Database is marked failed and every later operation returns `CommitOutcomeUnknown` until it is reopened and Recovery decides. A failed fsync is never retried: after a failure the OS may have dropped the dirty pages and a second fsync can report success without writing them (PostgreSQL, 2018).
- **One fsync per commit.** There is no relaxed mode; COMMITTED always means Durable. Group commit (v0.6) will recover throughput.
- **Unbounded growth.** Without checkpoints (v0.4) no Record can be dropped, and Recovery replays the whole file on every open.
- After Recovery, `last_committed` is the highest Commit Timestamp replayed or recorded in `applied_up_to`, so new Commit Timestamps continue the sequence.
