# psdb-storage-memory

`MemoryEngine`: a `StorageEngine` backed by a `BTreeMap` behind a `RwLock`. Nothing reaches disk.

**Depends on:** `psdb-storage`. **Depended on by:** `psdb-cli`, and `psdb-core`'s tests.
