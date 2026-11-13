# Background Index builds (Building → Ready)

Status: resolved
Blocked by: 01

Spec: [Index builds](../spec.md#index-builds), [ADR 0013](../../../docs/adr/0013-building-indexes-take-the-lock-per-chunk.md).

- Required `state` in the Catalog's `indexes` entries (no backward compatibility); delete the ID-skip loop in `prepare_index`.
- `IndexInfo.state: IndexState`; CLI `indexes` shows it.
- Writers maintain Building Indexes; the planner uses only Ready ones.
- The commit lock is taken per chunk, reading at the newest Commit Timestamp; each chunk checks the Collection ID and Index ID are still there and Building.
- Ready and cleanup commits re-read the newest entry under the lock and touch only their own Index.
- Unique builds treat an existing entry naming the same Document as already built.
- Dropping the Index or Collection ends the build with the new non-retryable `Error::IndexBuildCancelled`; any other failure removes the entry in one commit and returns the original error. Recovery drops Building Indexes in one commit.
- Tests: a Transaction whose Snapshot predates the Building commit aborts with Collection Changed (ADR 0013); a long build while other threads commit; drop during a build (Index, and Collection); two concurrent builds on different fields of one Collection both end Ready; a writer updates a Document ahead of a unique build's cursor and the build still succeeds; crash during a build.
- Update `ARCHITECTURE.md` for the changed flows and invariants.

## Comments

**Lock fairness between chunks (2026-11-13).** `IndexBuild::step` takes the commit lock and releases it on return, so a build holds the lock for one chunk at a time, not the whole build. But `std::sync::Mutex` is unfair: after unlocking, the build thread can retake the lock before a woken writer is scheduled. The `std::thread::yield_now()` in `create_index` makes this less likely but doesn't prevent it, so in theory a writer can wait out the whole build. Nobody has seen that happen. If a test or benchmark shows writers starving behind a build, end `step` with `parking_lot::MutexGuard::unlock_fair` to hand the lock straight to a waiter. That needs the `parking_lot` dependency. A hand-rolled ticket lock in front of `commit_lock` would avoid the dependency but is more code. Until then, keep `yield_now`; the `shortcut:` comment in `create_index` marks it.
