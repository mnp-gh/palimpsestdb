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
A named, schemaless group of Documents within a Namespace.
_Avoid_: Table, bucket

**Namespace**:
A named group of Collections, and the unit to which permissions are granted.
_Avoid_: Database (the Database Layer is the software, not a group of Collections), Schema (that is the Collection Schema), keyspace, tenant

**Collection path**:
The name of a Collection together with the name of its Namespace, written `<namespace>.<collection>` (e.g. `shop.orders`); it identifies one Collection on the Node.
_Avoid_: Namespace (MongoDB's word for it, but a Namespace here is only the first part), full name, qualified name

**Catalog**:
The authoritative record of which Namespaces and Collections exist and how their names map to their identities.
_Avoid_: Metadata store, schema registry

**Catalog entry**:
The Catalog's record of one Namespace or one Collection: its name and identity, and for a Collection its Collection Schema and Indexes.
_Avoid_: Metadata row

**Index**:
A secondary lookup over one field of a Collection's Documents, covering values of its field's Field Type, consistent with those Documents at every Snapshot. A unique Index also forbids two Documents from sharing a value in that field. While it is Building, writes keep it up to date (and enforce its uniqueness) but queries do not use it; it becomes Ready when its build finishes.
_Avoid_: Secondary key, lookup table

**Collection Schema**:
The optional declarations a Collection makes about its fields; for now, each field's Field Type and whether it is a Strict Field. Collections are schemaless, so it exists mainly to give Indexes their types: an Index reads its field's type from it, and it knows nothing about Indexes. A Collection without one accepts any Document, and Documents are never changed to fit it.
_Avoid_: Index schema, validator

**Field path**:
The dotted path to a field inside a Document, such as `address.city`: each segment names a field of the embedded document before it. Filters compare fields, the Collection Schema declares them, and Indexes cover them, by Field path. The Collection Schema never declares `_id` or a field under it, so no Index covers one, because Documents are already keyed by `_id`.
_Avoid_: Column, key path

**Field Type**:
The kind of value a field holds, declared in the Collection Schema. An Index has no type of its own: it covers values of its field's Field Type. Whether a Document is in the Index follows each of its Versions: while its field holds another kind of value it is left out, and a later write giving the field a value of the Field Type brings it in (and back out again if the type changes away). The Document itself is never changed.
_Avoid_: Index Type, column type

**Strict Field**:
A field whose Field Type the Collection Schema enforces: a write giving it a value of another type fails with Type Mismatch. Only a Strict Field can have a unique Index.
_Avoid_: Required field (a Strict Field may be missing), typed column

## Access

**User**:
An identity that can authenticate to a Node. Users belong to the Node, not to any one Namespace.
_Avoid_: Account, login, principal

**Role**:
A named set of permissions granted to a User on one Namespace (or Node-wide, for root). It belongs to that Namespace, not to its name: dropping the Namespace removes it.
_Avoid_: Permission group, grant

Tenants are not modelled; a tenant is a convention of Users holding Roles on a set of Namespaces.

## Layers

**Storage Engine**:
The replaceable bottom layer: an ordered store of byte keys to byte values with atomic batched writes. It knows nothing about documents, versions or transactions.
_Avoid_: Backend, driver, store

**Database Layer**:
Everything above the Storage Engine that gives bytes meaning: documents, versions, transactions, durability and isolation.
_Avoid_: Document Layer (Documents are one thing it manages, not all), engine (ambiguous with Storage Engine), core

## Transactions

**Transaction**:
A unit of reads and writes that commits or aborts as a whole.

**Snapshot**:
The consistent view of every Namespace on the Node that a Transaction reads, fixed at the moment the Transaction begins.

**Write Set**:
The writes a Transaction has made but not yet committed (Documents, their Index entries and Catalog entries); visible only to that Transaction.
_Avoid_: Buffer, pending writes

**Commit Timestamp**:
A counter, not clock time, that orders committed Transactions: each commit takes the next value, so no two share one. A committed Transaction's writes become visible to Snapshots taken at or after it.
_Avoid_: Version number, LSN, wall-clock time

**Version**:
One committed state of a key (a Document, an Index entry or a Catalog entry), identified by its Commit Timestamp.
_Avoid_: Revision

**Tombstone**:
A Version that records a key's deletion: a deleted Document, a removed Index entry, or a dropped Namespace or Collection.
_Avoid_: Delete marker

**Snapshot Isolation**:
The isolation level where each Transaction reads from its Snapshot and aborts if it writes a document that a concurrent Transaction already committed (first-committer-wins).
_Avoid_: SI as a synonym for Serializable

**Serializable**:
The isolation level where every outcome matches some serial order of the committed Transactions. Achieved via Serializable Snapshot Isolation.
_Avoid_: Strict, strong (unqualified)

**Garbage Collection**:
Removing Versions that no active Snapshot can read any more: Versions older than the newest one each key had at the Horizon, and every Version under a dropped Collection or Index once the Horizon is past its drop. It never changes what any Transaction sees.
_Avoid_: Vacuum, compaction (that is a Storage Engine's own housekeeping), cleanup

**Horizon**:
The oldest Snapshot any active Transaction reads, or the newest Visible commit when no Transaction is active. Every Snapshot now open or taken later is at or after it, so Garbage Collection may remove whatever no Snapshot at or after the Horizon can read. It only moves forward.
_Avoid_: Low-water mark, oldest snapshot (when the Horizon is meant), watermark

**Read Marker**:
A record that a Transaction read a key, a range or a whole Collection, kept so Serializable can detect dangerous interleavings. It never blocks anyone.
_Avoid_: SIREAD lock, predicate lock, read lock

**Abort**:
The outcome of a Transaction whose writes are discarded. Aborts caused by concurrency are retryable; the reasons are Write Conflict, Serialization Failure and Collection Changed.
_Avoid_: Rollback (that is what the client asks for; Abort is the outcome)

**Write Conflict**:
An Abort because a concurrent Transaction committed a write to the same key first: the same Document, the same value in a unique Index, or the same Catalog entry.

**Serialization Failure**:
An Abort because committing would produce an outcome not equivalent to any serial order.

**Collection Changed**:
An Abort because, after the Transaction's Snapshot, a Collection it used was dropped, renamed, or had its Collection Schema or Indexes changed; or a Document was committed into a Collection whose every Document it relied on (a schema change, or an Index created on an empty Collection).

**Commit Outcome Unknown**:
The result of a commit whose durability could not be confirmed: the Transaction may or may not survive a restart. Not an Abort, and not safely retryable.
_Avoid_: Failed commit, I/O error

**Transaction Too Large**:
The failure of a Transaction whose Write Set grows past the configured cap. Not an Abort, and not retryable: the same Transaction would fail again.
_Avoid_: Overflow, out of memory

**Duplicate Value**:
The failure of a write that would give two Documents the same value in a unique Index. Not an Abort, and not retryable.
_Avoid_: Constraint violation, duplicate key

**Type Mismatch**:
The failure of a write that gives a Strict Field a value of another type. Not an Abort, and not retryable.
_Avoid_: Schema violation, validation error

**Index Build Cancelled**:
The failure of an Index build whose Index or Collection was dropped while it ran. Not an Abort, and not retryable: retrying would undo the drop or find no Collection.
_Avoid_: Build aborted

**Node**:
One running instance of PalimpsestDB. Until replication exists, PalimpsestDB runs as exactly one Node.
_Avoid_: Server, instance, replica (until replication is designed), database (a Node holds many Namespaces)

## Commit lifecycle

**WAL**:
The append-only log of every committed Transaction's writes, in Commit Timestamp order. A Transaction is Durable once its WAL Record is on disk.
_Avoid_: Journal, redo log, commit log

**WAL Record**:
One committed Transaction in the WAL: its Commit Timestamp and Write Set. Aborted and read-only Transactions have none.
_Avoid_: Log entry, journal entry

**Recovery**:
Replaying the WAL into the Storage Engine when a Node opens, then dropping every Index still Building, since no build survives a restart. A torn last WAL Record is cut off; damage anywhere else stops the Node from opening rather than lose Durable Transactions.
_Avoid_: Restore, repair

**Durable**:
A Transaction is Durable once its commit is guaranteed to survive a crash.

**Applied**:
A Transaction is Applied once its Write Set is in the Storage Engine. Applied does not imply Durable.
_Avoid_: Flushed, persisted

**Visible**:
A Transaction is Visible once new Snapshots include its writes. A client is told COMMITTED only after its Transaction is Durable and Visible.
