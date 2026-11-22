# Read-only commits take the commit lock

Status: resolved

Autocommit reads (`Node::get`, `Node::find`, `list_*`) run through `Node::run` (`node.rs:325`), which calls `Transaction::commit` (`transaction.rs:394`). `commit` takes the commit lock before it checks `write_set.is_empty()` (`transaction.rs:409`). So a read-only `get` waits while another thread holds the lock for its fsync (several ms with `F_FULLFSYNC` on macOS), or for an Index build chunk.

Under concurrent writes, every read gets as slow as the disk, which undoes most of what v0.4 concurrency gives mixed workloads.

Fix: move `CommitState::poisoned` to an `AtomicBool` on `Node`, then in `commit` check poisoned and an empty Write Set before taking the lock. Keeps the contract that a read-only commit fails with `Poisoned` after a poisoning. About ten lines.

Test: hold the commit lock on one thread; `node.get` on another returns without waiting.
