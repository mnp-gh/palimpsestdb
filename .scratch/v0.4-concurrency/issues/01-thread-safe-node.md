# Thread-safe Node, visibility watermark, Snapshot registry

Status: resolved

Spec: [Threads](../spec.md#threads).

- `begin_transaction()` reads `latest_visible_commit_ts: AtomicU64` instead of taking the commit lock; the commit stores it after the apply. Checks `commits.is_poisoned()` and fails fast.
- Snapshot registry `Mutex<BTreeMap<u64, usize>>` with its own lock; Transactions unregister via `Drop`; `Node::horizon()` for Garbage Collection.
- Compile-time `Send + Sync` assertions; a stress test on real threads.
- Update `psdb-core/src/lib.rs`'s "single-threaded in intent" note.
