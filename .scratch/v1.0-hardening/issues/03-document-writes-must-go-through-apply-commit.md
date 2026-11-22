# Every Document write must go through `apply_commit`

Status: needs-triage

The Collection Changed check for whole-Collection reads (a Strict field check, Index creation on an empty Collection) relies on `CommitState::collection_updated_at`, which only `CommitState::apply_commit` updates. A Document written to the engine any other way is invisible to that check, so a concurrent Strict change could commit over a Document it never validated.

Nothing enforces this. Today the only other engine writes are safe:

- WAL replay on open (`node.rs`), before any Snapshot exists.
- Garbage Collection (`gc.rs`), which only removes Versions no Snapshot can see.

A future bulk-load, import, repair or replication path that writes Documents straight to the engine would silently break the check.

Open questions:

- Enforce it in code (e.g. only `apply_commit` can build a batch containing Document keys), or cover it with a test that any new engine-write path must pass?
- Is a line in ARCHITECTURE.md's commit section enough until such a path is planned?
