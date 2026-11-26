# Code conventions

How PalimpsestDB code is shaped. Vocabulary follows [GLOSSARY.md](../GLOSSARY.md); scope per version follows [ROADMAP.md](./ROADMAP.md).

What a machine can check is checked: `cargo clippy --all-targets -- -D warnings` (lints in `Cargo.toml`, seam bans in `clippy.toml`) `scripts/check-crates.sh` (crate names, READMEs, dependency direction) and `psdb-lint` (no TODO markers or commented-out code). `scripts/lint.sh` runs them all; a Claude Code hook runs it after every edit, and CI on every pull request. The rest of this file is for review.

## SOLID, applied at the seams

SOLID is applied to modules and crates, and strictly only to substitutability.

- **Single responsibility**: one module or crate per reason to change (storage, MVCC, transactions, WAL, documents, query, server). Inside a module, plain structs and functions.
- **Open/closed**: traits for extension points that outsiders implement; enums for closed sets (value types, transaction states) so `match` stays exhaustive.
- **Liskov substitution (strict)**: every trait's contract is written as doc comments on the trait (ordering, atomicity, crash behaviour), and every implementation passes that trait's conformance suite.
- **Interface segregation**: split a trait only when a real caller needs a subset (e.g. read-only access).
- **Dependency inversion**: generics at the real seams only — Storage Engine, concurrency control, WAL, Clock, I/O, and later the replication transport. Clock and I/O exist so the deterministic simulator can drive the system.

## Choosing a Storage Engine

The Database Layer is generic over the Storage Engine (`Node<E: StorageEngine>`), monomorphised and zero-cost. The binary chooses once, at its entry point, from configuration, via an enum or `Box<dyn StorageEngine>`.

## Crates

- Named `psdb-<area>`; split at the seams above.
- Dependencies point down: engine crates depend only on `psdb-storage`; `psdb-core` depends on `psdb-storage` and names a concrete engine only in tests and binaries.

## Code layout

- Files read top-down: a function comes before the helpers it calls, and each helper sits just below its first caller. A helper that only serves one `impl` goes inside it as an associated function, so it stays next to its caller.

## Documentation

- `#![deny(missing_docs)]` in every crate.
- Every public function returning `Result` has an `# Errors` section, and every one that can panic a `# Panics` section (clippy `missing_errors_doc`, `missing_panics_doc`). Each lists only what is particular to the function; never copy a list:
  - What nearly every call can return (Storage, Corruption, Transaction Too Large, no such Namespace or Collection) is written once, on `Error` under "Errors every operation can return". Lock poisoning is written once, on `Node` under "Panics".
  - A wrapper points to what it wraps: an autocommit method says ``Those of [`Transaction::insert`], then of [`Transaction::commit`].`` A trait method points to its trait's section.
  - A function with nothing of its own links to the shared section in one line.
- A method that returns a new value of its own type instead of changing `self` (a builder like `Limits::with_max_document_bytes`, or `KeyRange::intersect`) is `#[must_use]` (clippy `return_self_not_must_use`), so dropping the result warns instead of silently doing nothing.
- A `//` comment says why, which the code cannot: an invariant, a reason, a link to an ADR. Never what the code does; rename or restructure until the code says it. Deferred work is `// shortcut: <the limit>, <when to upgrade>` or a roadmap item, never `TODO`. The hook shows Claude each comment it adds, to check.
- A README per crate stating its responsibility and its place in the dependency graph.
- Doc examples are tests (`cargo test` runs them).
- Domain terms in code, comments and tests use the glossary's words.
- [ARCHITECTURE.md](../ARCHITECTURE.md) maps the code. Update it in the same change as a new crate or module, a changed flow, or a changed invariant.

## Testing

- Conformance suite per seam trait, run against every implementation.
- Property tests with `proptest` from v0.1.
- Crash tests from v0.2; deterministic simulator from v0.4; Elle-style history checker from v0.5.
- A test must be able to fail. Its expected value never comes from the code under test: use a model, a hand-written known answer, or a second implementation. Every on-disk format has a test with its bytes written out by hand from the spec.
- Randomized tests must reach the behaviour they claim to test: a generator that rarely produces the interesting case (a duplicate, a full Index) is a coverage gap even when the test passes.
- When a change adds a test, run `cargo mutants --in-diff <(git diff origin/main...)` to check the test catches changes to the code it covers. Only then: each mutant reruns the tests, so it runs neither in CI nor on every pull request. A mutant no test notices is a missing test; one that cannot change behaviour goes in `.cargo/mutants.toml` with a comment saying why.
