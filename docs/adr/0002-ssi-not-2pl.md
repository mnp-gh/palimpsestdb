# Serializable via SSI on top of Snapshot Isolation, not 2PL with predicate locks

Concurrency control is MVCC with Snapshot Isolation first, then Serializable Snapshot Isolation (Cahill/Ports, as in PostgreSQL) to reach Serializable. Two-phase locking with predicate locks was in the original wishlist but is deferred: running two independent concurrency-control schemes doubles the correctness surface, and in a document database predicate locks degenerate into index-range or whole-collection locks. It may return later as an experiment behind a concurrency-control trait.
