# Making a field Strict on a busy Collection

Status: needs-triage

Since v0.4 issue 07, making a field Strict checks the existing Documents inside its Transaction, and its commit aborts with Collection Changed if any Document was committed into the Collection after its Snapshot. On a Collection that gets Documents all the time, one almost always lands during the check, so the change can abort on every retry.

Goal: making a field Strict succeeds on a busy Collection.

Open questions:

- Should it work like a background Index build (ADR 0013): mark the field as becoming Strict, have writes enforce it from then on, check the existing Documents in chunks, then a short final commit?
- What happens to a Document that fails the check part-way: the change fails like a Duplicate Value build, or the field stays not Strict?
- Does Index creation on an empty Collection need the same, or is retrying enough there?
