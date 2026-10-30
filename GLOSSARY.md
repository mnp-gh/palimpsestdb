# PalimpsestDB

A document database built as a learning and research project: correctness and clarity first, performance second.

## Data

**Document**:
A single BSON object stored in a Collection; the unit of reading and writing.
_Avoid_: Record, row, object

**Document ID**:
The `_id` of a Document: unique within its Collection, supplied by the client or generated if absent, and always a BSON scalar (never an array, document or null).
_Avoid_: Primary key, key, oid

**Collection**:
A named, schemaless group of Documents within a Database.
_Avoid_: Table, bucket

**Database**:
A named namespace of Collections, and the unit to which permissions are granted.
_Avoid_: Schema, keyspace, tenant

**Catalog**:
The authoritative record of which Databases and Collections exist and how their names map to their identities.
_Avoid_: Metadata store, schema registry

**Index**:
A secondary lookup over one field of a Collection's Documents, consistent with those Documents at every Snapshot.
_Avoid_: Secondary key, lookup table

## Access

**User**:
An identity that can authenticate to a Node. Users belong to the Node, not to any one Database.
_Avoid_: Account, login, principal

**Role**:
A named set of permissions granted to a User on one Database (or Node-wide, for root). It belongs to that Database, not to its name: dropping the Database removes it.
_Avoid_: Permission group, grant

Tenants are not modelled; a tenant is a convention of Users holding Roles on a set of Databases.

## Layers

**Storage Engine**:
The replaceable bottom layer: an ordered store of byte keys to byte values with atomic batched writes. It knows nothing about documents, versions or transactions.
_Avoid_: Backend, driver, store

**Document Layer**:
Everything above the Storage Engine that gives bytes meaning: documents, versions, transactions, durability and isolation.
_Avoid_: Database Layer ("Database" is the namespace), engine (ambiguous with Storage Engine), core

## Transactions

**Transaction**:
A unit of reads and writes that commits or aborts as a whole.

**Snapshot**:
The consistent view of every Database on the Node that a Transaction reads, fixed at the moment the Transaction begins.

**Write Set**:
The Documents a Transaction has written but not yet committed; visible only to that Transaction.
_Avoid_: Buffer, pending writes

**Commit Timestamp**:
A counter, not clock time, that orders committed Transactions: each commit takes the next value, so no two share one. A committed Transaction's writes become visible to Snapshots taken at or after it.
_Avoid_: Version number, LSN, wall-clock time

**Version**:
One committed state of a Document, identified by its Commit Timestamp.
_Avoid_: Revision

**Snapshot Isolation**:
The isolation level where each Transaction reads from its Snapshot and aborts if it writes a document that a concurrent Transaction already committed (first-committer-wins).
_Avoid_: SI as a synonym for Serializable

**Serializable**:
The isolation level where every outcome matches some serial order of the committed Transactions. Achieved via Serializable Snapshot Isolation.
_Avoid_: Strict, strong (unqualified)

**Read Marker**:
A record that a Transaction read a key, a range or a whole Collection, kept so Serializable can detect dangerous interleavings. It never blocks anyone.
_Avoid_: SIREAD lock, predicate lock, read lock

**Abort**:
The outcome of a Transaction whose writes are discarded. Aborts caused by concurrency are retryable; the reasons are Write Conflict, Serialization Failure and Collection Changed.
_Avoid_: Rollback (that is what the client asks for; Abort is the outcome)

**Write Conflict**:
An Abort because a concurrent Transaction committed a write to the same Document first.

**Serialization Failure**:
An Abort because committing would produce an outcome not equivalent to any serial order.

**Collection Changed**:
An Abort because a Collection the Transaction used was dropped, renamed or re-indexed after its Snapshot.

**Commit Outcome Unknown**:
The result of a commit whose durability could not be confirmed: the Transaction may or may not survive a restart. Not an Abort, and not safely retryable.
_Avoid_: Failed commit, I/O error

**Node**:
One running instance of PalimpsestDB. Until replication exists, PalimpsestDB runs as exactly one Node.
_Avoid_: Server, instance, replica (until replication is designed)

## Commit lifecycle

**WAL**:
The append-only log of every committed Transaction's writes, in Commit Timestamp order. A Transaction is Durable once its WAL Record is on disk.
_Avoid_: Journal, redo log, commit log

**WAL Record**:
One committed Transaction in the WAL: its Commit Timestamp and Write Set. Aborted and read-only Transactions have none.
_Avoid_: Log entry, journal entry

**Recovery**:
Replaying the WAL into the Storage Engine when a Node opens. A torn last WAL Record is cut off; damage anywhere else stops the Node from opening rather than lose Durable Transactions.
_Avoid_: Restore, repair

**Durable**:
A Transaction is Durable once its commit is guaranteed to survive a crash.

**Applied**:
A Transaction is Applied once its Write Set is in the Storage Engine. Applied does not imply Durable.
_Avoid_: Flushed, persisted

**Visible**:
A Transaction is Visible once new Snapshots include its writes. A client is told COMMITTED only after its Transaction is Durable and Visible.
