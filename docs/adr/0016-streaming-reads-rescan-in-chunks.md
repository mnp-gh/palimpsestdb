# Streaming reads rescan the engine in chunks

`Transaction::find` returns an iterator, but the Storage Engine trait keeps its push-style `scan(range, visit)`. Each chunk is one `scan` at the Transaction's Snapshot that stops after a set number of Documents or bytes; the next chunk starts just past the last Document's Versions. A Snapshot's Versions cannot change while it is registered, so rescanning from a resume key returns exactly what one long scan would.

## Considered Options

- **Pull iterators in the Storage Engine trait** (RocksDB-style): no re-seek per chunk, but every engine must return an iterator that pins its own state, and the in-memory engine would hold its lock for as long as the caller iterates.

## Consequences

- Each chunk pays one seek to its start key; chunk sizes (`Limits`) keep that small next to the chunk's work.
- Only an explicit Transaction streams; `Node::find` collects into a `Vec` because its Transaction ends before it returns (as PostgreSQL's JDBC driver streams only outside autocommit). A cursor that owns its Transaction can come with the v0.6 server.
