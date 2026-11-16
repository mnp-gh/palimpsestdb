# Making a field Strict without blocking other commits

Status: resolved

Making a field Strict (`crates/psdb-core/src/schema.rs:213`) and creating an Index on an empty Collection (`crates/psdb-core/src/index.rs:300`) record a dependency on the whole Collection. Checking the existing Documents runs inside the Transaction, without the commit lock. But at commit, `mvcc::changed_since` reads every Version ever written under the Collection, Documents and Index entries, while holding the commit lock, so every other commit waits for a scan that grows with the Collection.

The check only needs to know whether a Document was committed into the Collection after the Snapshot.

- Keep in memory, per Collection, the Commit Timestamp of the last commit that wrote a Document into it, updated in `apply_commit` under the commit lock. Index entries (build chunks included) do not update it.
- The commit-time check for `collection_content_reads` compares that timestamp with the Snapshot instead of scanning, and aborts with Collection Changed if it is newer. No entry means no Document was written since the Node opened, and every Snapshot is taken after that, so the check passes. Nothing is persisted or logged.
- Entries for dropped Collections stay: a few bytes each.
- `shortcut:` comment at the check: on a Collection that gets Documents all the time, a Strict change can abort every retry, because one lands while the existing Documents are checked; upgrade to the gradual design (`.scratch/v1.0-hardening/issues/01-gradual-strict-field.md`) if that bites.
- Remove `mvcc::changed_since` if nothing else uses it.
- Tests: a Document committed into the Collection while a Strict change is open aborts it with Collection Changed; a Document committed into another Collection, or a build chunk's Index entries, does not.
- Update `ARCHITECTURE.md` for the changed check.

The gradual design (Strict enforced by writers while existing Documents are checked in chunks, like a background Index build) is deferred to v1.0 Hardening.
