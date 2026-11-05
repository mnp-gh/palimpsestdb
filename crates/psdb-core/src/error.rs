//! Errors returned by the Database Layer.

use std::fmt;

use psdb_storage::StorageError;

use crate::collection_path::CollectionPath;
use crate::field_path::FieldPath;

/// Result type for Database Layer operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong in the Database Layer.
#[derive(Debug)]
pub enum Error {
    /// Abort: a concurrent Transaction committed a write to the same key
    /// first (first-committer-wins). Retryable.
    WriteConflict,
    /// Abort: a Collection or Namespace this Transaction used was dropped or
    /// changed after its Snapshot (ADR 0006). Retryable.
    CollectionChanged,
    /// No Namespace with this name exists at the Transaction's Snapshot.
    NoSuchNamespace(String),
    /// No Collection with this name exists in the Namespace at the
    /// Transaction's Snapshot.
    NoSuchCollection(CollectionPath),
    /// A Namespace with this name already exists.
    NamespaceExists(String),
    /// A Collection with this name already exists in the Namespace.
    CollectionExists(CollectionPath),
    /// A Document with this Document ID already exists in the Collection.
    DuplicateId(String),
    /// Duplicate Value: the write would give two Documents the same value
    /// in a unique Index. Not an Abort, and not retryable.
    DuplicateValue(String),
    /// Type Mismatch: the write gives a Strict Field a value of another
    /// type. Not an Abort, and not retryable.
    TypeMismatch(String),
    /// A Collection Schema change or Index definition breaks a rule, such
    /// as indexing a field whose type is not declared.
    InvalidSchema(String),
    /// The Collection already has an Index on this field.
    IndexExists {
        /// The Collection.
        collection: CollectionPath,
        /// The Field path.
        path: FieldPath,
    },
    /// The Collection has no Index on this field.
    NoSuchIndex {
        /// The Collection.
        collection: CollectionPath,
        /// The Field path.
        path: FieldPath,
    },
    /// An Index can only be created inside a Transaction while its
    /// Collection is empty; otherwise use
    /// [`Node::create_index`](crate::Node::create_index), which
    /// builds it on its own (ADR 0010).
    CollectionNotEmpty(CollectionPath),
    /// A Namespace or Collection name breaks the naming rules.
    InvalidName(String),
    /// A value cannot be used as a Document ID.
    InvalidId(String),
    /// A Document breaks a rule or limit.
    InvalidDocument(String),
    /// A filter cannot be parsed.
    InvalidFilter(String),
    /// Stored data could not be decoded.
    Corruption(String),
    /// The Storage Engine failed.
    Storage(StorageError),
    /// Reading or writing the WAL failed while opening a Node.
    Io(std::io::Error),
    /// The WAL file has the wrong magic number or an unknown format
    /// version. It was left untouched.
    UnrecognisedWal(String),
    /// Transaction Too Large: the write would grow the Write Set past
    /// [`Limits::max_write_set_bytes`](crate::Limits::max_write_set_bytes)
    /// (the write is not made), or the WAL Record would exceed 4 GiB
    /// (nothing is committed). Not an Abort, and not retryable.
    TransactionTooLarge,
    /// [`Node::open`](crate::Node::open) was given an engine that
    /// already holds data; the WAL is replayed into an empty engine.
    EngineNotEmpty,
    /// The commit could not be confirmed Durable, or was Durable but could
    /// not be Applied: the Transaction may or may not be present after the
    /// Node is reopened. Not an Abort, and not retryable. The Node
    /// is now poisoned.
    CommitOutcomeUnknown(Box<Error>),
    /// An earlier commit ended in [`Error::CommitOutcomeUnknown`], so
    /// nothing more can commit until the Node is reopened.
    Poisoned,
}

impl Error {
    /// Whether retrying the whole Transaction may succeed. True for Aborts
    /// caused by concurrency.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Error::WriteConflict | Error::CollectionChanged)
    }

    /// Whether the Node can no longer commit until it is reopened.
    pub fn requires_reopen(&self) -> bool {
        matches!(self, Error::CommitOutcomeUnknown(_) | Error::Poisoned)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::WriteConflict => {
                f.write_str("write conflict: a concurrent transaction committed first; retry")
            }
            Error::CollectionChanged => {
                f.write_str("collection changed: a collection used by this transaction was dropped or altered; retry")
            }
            Error::NoSuchNamespace(namespace) => write!(f, "namespace {namespace:?} does not exist"),
            Error::NoSuchCollection(collection) => {
                write!(f, "collection {collection} does not exist")
            }
            Error::NamespaceExists(namespace) => write!(f, "namespace {namespace:?} already exists"),
            Error::CollectionExists(collection) => {
                write!(f, "collection {collection} already exists")
            }
            Error::DuplicateId(id) => write!(f, "a document with _id {id} already exists"),
            Error::DuplicateValue(msg) => write!(f, "duplicate value: {msg}"),
            Error::TypeMismatch(msg) => write!(f, "type mismatch: {msg}"),
            Error::InvalidSchema(msg) => write!(f, "invalid schema: {msg}"),
            Error::IndexExists { collection, path } => {
                write!(f, "collection {collection} already has an index on {path}")
            }
            Error::NoSuchIndex { collection, path } => {
                write!(f, "collection {collection} has no index on {path}")
            }
            Error::CollectionNotEmpty(collection) => write!(
                f,
                "collection {collection} holds documents, so its index must be built outside a transaction"
            ),
            Error::InvalidName(msg) => write!(f, "invalid name: {msg}"),
            Error::InvalidId(msg) => write!(f, "invalid _id: {msg}"),
            Error::InvalidDocument(msg) => write!(f, "invalid document: {msg}"),
            Error::InvalidFilter(msg) => write!(f, "invalid filter: {msg}"),
            Error::Corruption(msg) => write!(f, "corrupted data: {msg}"),
            Error::Storage(e) => write!(f, "{e}"),
            Error::Io(e) => write!(f, "WAL I/O error: {e}"),
            Error::UnrecognisedWal(msg) => write!(f, "unrecognised WAL: {msg}"),
            Error::TransactionTooLarge => f.write_str(
                "transaction too large: its write set would exceed the configured limit or its WAL record 4 GiB",
            ),
            Error::EngineNotEmpty => {
                f.write_str("the storage engine already holds data; open a WAL into an empty engine")
            }
            Error::CommitOutcomeUnknown(cause) => write!(
                f,
                "commit outcome unknown ({cause}); the transaction may or may not survive a reopen, and the node must be reopened"
            ),
            Error::Poisoned => f.write_str(
                "an earlier commit's outcome is unknown, so nothing more can commit; reopen the node",
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Storage(e) => Some(e),
            Error::Io(e) => Some(e),
            Error::CommitOutcomeUnknown(cause) => Some(cause.as_ref()),
            _ => None,
        }
    }
}

impl From<StorageError> for Error {
    fn from(e: StorageError) -> Self {
        Error::Storage(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
