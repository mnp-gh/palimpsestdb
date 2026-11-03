# psdb-core

The Document Layer: Databases, Collections and the transactional Catalog; BSON Documents; single-field Indexes over a Collection Schema (ADR 0010, ADR 0011); Versions encoded into engine keys (ADR 0008); Transactions under Snapshot Isolation with first-committer-wins; durability through a WAL (ADR 0004).

```rust
use bson::doc;
use psdb_core::Database;
use psdb_storage_memory::MemoryEngine;

// Durable: the WAL lives in ./data and is replayed on every open.
let db = Database::open(MemoryEngine::new(), "data")?;
db.create_database("shop")?;
db.create_collection("shop", "orders")?;

let mut txn = db.begin();
txn.insert("shop", "orders", doc! { "_id": 1, "item": "book" })?;
txn.commit()?;
```

| Module | Responsibility |
|---|---|
| `database` | `Database`: begin Transactions, autocommit helpers |
| `transaction` | Snapshot reads, Write Set, commit-time conflict checks |
| `catalog` | Databases, Collections, Collection Schemas and Indexes as versioned Catalog entries |
| `index` | Index Types, Strict Fields, Index entry keys, choosing an Index or `_id` range for a filter |
| `mvcc` | Which Version of a key a Snapshot sees |
| `keys` | Order-preserving key encoding |
| `document` | Limits, Document IDs, name rules |
| `filter` | Parsing and evaluating `find` filters |
| `wal` | WAL records, recovery, and the `WalFile` I/O seam with its `SimDisk` fake |

**Depends on:** `psdb-storage` (never on a concrete engine outside tests). **Depended on by:** `psdb-cli`.
