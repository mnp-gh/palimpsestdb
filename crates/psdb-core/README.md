# psdb-core

The Database Layer: Namespaces, Collections and the transactional Catalog; BSON Documents; single-field Indexes over a Collection Schema (ADR 0010, ADR 0011); Versions encoded into engine keys (ADR 0008); Transactions under Snapshot Isolation with first-committer-wins; durability through a WAL (ADR 0004).

```rust
use bson::doc;
use psdb_core::Node;
use psdb_storage_memory::MemoryEngine;

// Durable: the WAL lives in ./data and is replayed on every open.
let node = Node::open(MemoryEngine::new(), "data")?;
node.create_namespace("shop")?;
let orders = "shop.orders".parse()?;
node.create_collection(&orders)?;

let mut txn = node.begin();
txn.insert(&orders, doc! { "_id": 1, "item": "book" })?;
txn.commit()?;
```

| Module | Responsibility |
|---|---|
| `node` | `Node`: begin Transactions, autocommit helpers |
| `transaction` | Snapshot reads, Write Set, commit-time conflict checks |
| `catalog` | Namespaces, Collections, Collection Schemas and Indexes as versioned Catalog entries |
| `collection_path` | `CollectionPath`: a Namespace and Collection name, `shop.orders` |
| `field_path` | `FieldPath`: a dotted path to a declared or indexed field, `address.city` |
| `schema` | The Collection Schema: Field Types and Strict Fields |
| `index` | Index entry keys, choosing an Index or `_id` range for a filter |
| `mvcc` | Which Version of a key a Snapshot sees |
| `keys` | Order-preserving key encoding |
| `document` | Limits, Document IDs, name rules |
| `filter` | Parsing and evaluating `find` filters |
| `wal` | WAL records, recovery, and the `WalFile` I/O seam with its `SimDisk` fake |

**Depends on:** `psdb-storage` (never on a concrete engine outside tests). **Depended on by:** `psdb-cli`.
