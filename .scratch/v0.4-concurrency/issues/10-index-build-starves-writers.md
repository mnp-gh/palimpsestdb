# Index builds can starve writers

Status: ready-for-agent
Priority: fix before v0.5 starts

`Node::create_index` builds in chunks, releasing the commit lock between them so other commits get through (ADR 0013). Between chunks it only calls `std::thread::yield_now()` (`index.rs`, the `shortcut:` comment). std's `Mutex` makes no fairness promise, so the build can take the lock straight back before a waiting writer runs: writers then wait for the whole build, which is what background builds exist to prevent.

## Evidence

`tests/threads.rs` `other_threads_commit_while_an_index_builds` (four writers commit in a loop during a 10,000-Document build; at least one must get through) failed in CI on PR #15, an unrelated change.

- Locally it fails in about 11% of runs under CPU contention (16 copies at once, 9 of 80 failed) and never on an idle 8-core machine (0 of 100).
- Every failing run has the same signature: each of the four writers had started one commit attempt, and none completed or aborted during the whole build. Passing runs commit 460 to 1,200 times. So the writers were blocked on the commit lock, not slow to start.

The test is `#[ignore]`d until this is fixed; re-enable it as part of the fix.

## Fix

A hand-off in the build loop, without a new dependency: count commits waiting for the commit lock (increment before `lock()`, decrement once held), and after each chunk, if any were waiting when the build released the lock, wait until one of them has taken it before the next chunk. Bounded, so a stream of new writers cannot stall the build either.

A fair mutex (`parking_lot::FairMutex`) does the same with a new dependency and no lock poisoning, which the Panics docs and `begin_fails_fast_when_commit_lock_is_poisoned` rely on.

Done when the test is no longer ignored and passes 80 of 80 runs under the contention above, the `shortcut:` comment in `Node::create_index` is gone, and the ROADMAP item is removed.
