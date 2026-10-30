# psdb-wal

The write-ahead log (ADR 0004, ADR 0010): `Wal` appends and syncs one WAL Record per committed Transaction and, on open, returns every intact Record for Recovery, cutting off a torn tail. It writes through the `LogFile` trait, whose crash contract is documented on the trait; `FsLogFile` is the real file, and `FaultyLogFile` (feature `testing`) simulates crashes, torn writes and I/O failures.

**Depends on:** nothing in the workspace. **Depended on by:** `psdb-core`.
