# `psdb-wal` crate

Status: ready-for-agent

Spec: [../spec.md](../spec.md)

## What

A new workspace crate, `psdb-wal`, depending on nothing in the workspace, with `#![deny(missing_docs)]` and a README stating its place in the dependency graph.

- **`LogFile` trait**: append bytes, `sync_all`, read the whole file, truncate to a length. Its contract (what survives a crash: exactly the bytes synced) is documented on the trait.
- **`FsLogFile`**: the real implementation over `std::fs::File`, syncing with `File::sync_all` (`F_FULLFSYNC` on macOS).
- **`FaultyLogFile`** (test support, behind a feature or `cfg(test)` plus a `testing` feature for other crates): keeps synced and unsynced bytes apart; `crash()` drops the unsynced bytes, optionally keeping a random prefix of them to simulate a torn write; can be told to fail the next append or sync.
- **Record encoding and decoding** per D2, big-endian, CRC32C over the payload (add the `crc32c` crate).
- **`Wal<F: LogFile>`**: `append(&record)` then `sync()`, and `read_all()` returning the valid records plus the byte offset where the valid prefix ends, so the caller can truncate there.

## Tests

- Proptest: decode(encode(record)) == record.
- Proptest: for any sequence of records followed by any truncation or corruption of the tail, `read_all` returns exactly the records before the damage.

## Done when

`cargo test -p psdb-wal` passes and the crate is in the workspace.
