//! Errors returned by the Document Layer.

use std::fmt;

use psdb_storage::StorageError;

/// Result type for Document Layer operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong in the Document Layer.
#[derive(Debug)]
pub enum Error {
    /// Abort: a concurrent Transaction committed a write to the same key
    /// first (first-committer-wins). Retryable.
    WriteConflict,
    /// Abort: a Collection or Database this Transaction used was dropped or
    /// changed after its Snapshot (ADR 0006). Retryable.
    CollectionChanged,
    /// No Database with this name exists at the Transaction's Snapshot.
    NoSuchDatabase(String),
    /// No Collection with this name exists in the Database at the
    /// Transaction's Snapshot.
    NoSuchCollection {
        /// The Database name.
        database: String,
        /// The Collection name.
        collection: String,
    },
    /// A Database with this name already exists.
    DatabaseExists(String),
    /// A Collection with this name already exists in the Database.
    CollectionExists {
        /// The Database name.
        database: String,
        /// The Collection name.
        collection: String,
    },
    /// A Document with this Document ID already exists in the Collection.
    DuplicateId(String),
    /// A Database or Collection name breaks the naming rules.
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
}

impl Error {
    /// Whether retrying the whole Transaction may succeed. True for Aborts
    /// caused by concurrency.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Error::WriteConflict | Error::CollectionChanged)
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
            Error::NoSuchDatabase(db) => write!(f, "database {db:?} does not exist"),
            Error::NoSuchCollection { database, collection } => {
                write!(f, "collection {database}.{collection} does not exist")
            }
            Error::DatabaseExists(db) => write!(f, "database {db:?} already exists"),
            Error::CollectionExists { database, collection } => {
                write!(f, "collection {database}.{collection} already exists")
            }
            Error::DuplicateId(id) => write!(f, "a document with _id {id} already exists"),
            Error::InvalidName(msg) => write!(f, "invalid name: {msg}"),
            Error::InvalidId(msg) => write!(f, "invalid _id: {msg}"),
            Error::InvalidDocument(msg) => write!(f, "invalid document: {msg}"),
            Error::InvalidFilter(msg) => write!(f, "invalid filter: {msg}"),
            Error::Corruption(msg) => write!(f, "corrupted data: {msg}"),
            Error::Storage(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Storage(e) => Some(e),
            _ => None,
        }
    }
}

impl From<StorageError> for Error {
    fn from(e: StorageError) -> Self {
        Error::Storage(e)
    }
}
