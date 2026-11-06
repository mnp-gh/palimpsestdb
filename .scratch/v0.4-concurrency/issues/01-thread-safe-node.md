# Thread-safe Node, visibility watermark, Snapshot registry

Status: ready-for-agent

Spec: [Threads](../spec.md#threads).

- `begin()` reads an `AtomicU64` watermark instead of taking the commit lock; the commit stores it after the apply.
- Snapshot registry with its own lock; Transactions unregister on drop; `Node::horizon()` (or equivalent) for Garbage Collection.
- Compile-time `Send + Sync` assertions; a stress test on real threads.
- Update `psdb-core/src/lib.rs`'s "single-threaded in intent" note.
