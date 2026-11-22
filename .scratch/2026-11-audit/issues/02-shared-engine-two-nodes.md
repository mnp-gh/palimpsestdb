# Two live Nodes over one shared engine

Status: resolved

`impl StorageEngine for Arc<E>` (`psdb-storage/src/lib.rs:240`, added in v0.4.2) lets tests reopen a Node over the engine a dropped one used. Nothing stops two Nodes over the same `Arc` from being alive at once. Each hands out Commit Timestamps from its own `latest_visible_commit_ts`, so both write Versions at timestamp N and overwrite each other in the shared map. The WAL file lock does not help without a WAL, or with two WAL directories.

```rust
let e = Arc::new(MemoryEngine::new());
let a = Node::new(e.clone())?;
let b = Node::new(e.clone())?; // succeeds; a and b now corrupt each other
```

Options:

- Gate the `Arc` impl behind a `testing` feature on `psdb-storage`, since tests are its only users (smallest).
- Or have the Node claim the engine (an owned flag, released on drop) and fail `open` if already claimed.
