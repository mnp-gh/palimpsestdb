# Making a field Strict without blocking other commits

Status: needs-triage

Making a field Strict (`crates/psdb-core/src/schema.rs:213`) records a dependency on the whole Collection. At commit, `Transaction::commit` reads every Version ever written under the Collection to confirm no Document was committed after the Snapshot, while holding the commit lock, so every other commit waits for the whole scan. Index creation on an empty Collection (`crates/psdb-core/src/index.rs:300`) uses the same check, but there the Collection is empty, so the scan is short.

Garbage Collection (04, 05) shrinks what the scan reads, but it still runs under the commit lock and grows with the Collection's size.

Goal: making a field Strict should not stop other Transactions from committing for a time that grows with the Collection.

Open questions:

- Should it work like a background Index build (ADR 0013): mark the field as becoming Strict, have writes enforce it from then on, check the existing Documents in chunks, then a short final commit?
- What happens to a Document that fails the check part-way: the change fails like a Duplicate Value build, or the field stays not Strict?
- Can the commit-time check read only the Documents (skip Index entries), and only Versions after the Snapshot, instead of everything under the Collection?
