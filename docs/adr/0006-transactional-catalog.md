# The Catalog is transactional and versioned like Documents

The Catalog is stored as ordinary MVCC-versioned keys in reserved system Database 0, so DDL runs inside Transactions, commits or aborts atomically with data writes, and each Snapshot sees the Catalog as of its own timestamp. We rejected a separate non-transactional Catalog guarded by metadata locks (MySQL-style) because it makes DDL auto-commit, lets the Catalog change under an open Snapshot, and needs blocking locks that contradict our no-locks concurrency model.

## Consequences

- Snapshot Isolation alone does not catch "drop Collection" racing a write into it (that is read-write, not write-write). At commit, every Transaction checks that the Catalog entry of each Collection it touched is unchanged since its Snapshot; otherwise it aborts with a retryable `CollectionChanged`.
- Concurrent creation of the same name is an ordinary write-write conflict on the Catalog key.
- The Document Layer keeps an in-RAM cache of Catalog versions; the engine remains the source of truth.
- Databases and Collections are created explicitly. Unlike MongoDB, writing to a missing one is an error rather than an implicit create, so a typo such as `shop.oders` fails instead of silently creating a new Collection.
- Database, Collection and Index IDs come from a monotonic counter and are never reused, so stale keys of a dropped Collection can never surface in a new one.
