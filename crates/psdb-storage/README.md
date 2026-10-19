# psdb-storage

The `StorageEngine` trait: an ordered byte-key/byte-value store with atomic batched writes, and the contract every engine must honour (written on the trait itself). Also the conformance suite, `conformance::run_all`, that every engine crate runs in its tests.

**Depends on:** nothing. **Depended on by:** every engine crate and `psdb-core`.
