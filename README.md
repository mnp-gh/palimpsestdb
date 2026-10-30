# PalimpsestDB

A document database in Rust, built as a learning and research project: correctness and clarity first, performance second. New Versions are written over old ones, which stay readable to older Snapshots, like the scraped-and-rewritten pages of a palimpsest.

**Status:** v0.2 in progress: an embedded library with Snapshot Isolation, durable through a WAL over the in-memory Storage Engine. See [docs/ROADMAP.md](docs/ROADMAP.md).

## Try it

```
cargo run -p psdb-cli
cargo test --workspace
```

## Layout

| Crate | What it is |
|---|---|
| [`psdb-storage`](crates/psdb-storage) | The Storage Engine contract and its conformance suite |
| [`psdb-storage-memory`](crates/psdb-storage-memory) | In-memory Storage Engine |
| [`psdb-core`](crates/psdb-core) | The Document Layer: Catalog, Documents, MVCC, Transactions |
| [`psdb-cli`](crates/psdb-cli) | Interactive shell |

## Design docs

- [GLOSSARY.md](GLOSSARY.md): the project's vocabulary
- [docs/adr/](docs/adr/): architecture decisions
- [docs/CONVENTIONS.md](docs/CONVENTIONS.md): how the code is shaped
- [docs/ROADMAP.md](docs/ROADMAP.md): what each version adds
