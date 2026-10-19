# Code conventions

How PalimpsestDB code is shaped. Vocabulary follows [GLOSSARY.md](../GLOSSARY.md); scope per version follows [ROADMAP.md](./ROADMAP.md).

## SOLID, applied at the seams

SOLID is applied to modules and crates, and strictly only to substitutability.

- **Single responsibility**: one module or crate per reason to change (storage, MVCC, transactions, WAL, documents, query, server). Inside a module, plain structs and functions.
- **Open/closed**: traits for extension points that outsiders implement; enums for closed sets (value types, transaction states) so `match` stays exhaustive.
- **Liskov substitution (strict)**: every trait's contract is written as doc comments on the trait (ordering, atomicity, crash behaviour), and every implementation passes that trait's conformance suite.
- **Interface segregation**: split a trait only when a real caller needs a subset (e.g. read-only access).
- **Dependency inversion**: generics at the real seams only — Storage Engine, concurrency control, WAL, Clock, I/O, and later the replication transport. Clock and I/O exist so the deterministic simulator can drive the system.

## Choosing a Storage Engine

The core is generic over the engine (`Database<E: StorageEngine>`), monomorphised and zero-cost. The binary chooses once, at its entry point, from configuration, via an enum or `Box<dyn StorageEngine>`.

## Crates

- Named `psdb-<area>`; split at the seams above.
- Dependencies point down: engine crates depend only on `psdb-storage`; `psdb-core` depends on `psdb-storage` and names a concrete engine only in tests and binaries.

## Documentation

- `#![deny(missing_docs)]` in every crate.
- A README per crate stating its responsibility and its place in the dependency graph.
- Doc examples are tests (`cargo test` runs them).
- Domain terms in code, comments and tests use the glossary's words.

## Testing

- Conformance suite per seam trait, run against every implementation.
- Property tests with `proptest` from v0.1.
- Crash tests from v0.2; deterministic simulator from v0.4; Elle-style history checker from v0.5.
