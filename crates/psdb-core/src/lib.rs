//! The PalimpsestDB Database Layer.
//!
//! Everything above the Storage Engine that gives bytes meaning: Documents,
//! the Catalog of Namespaces and Collections, Versions, and Transactions
//! under Snapshot Isolation. See `GLOSSARY.md` for the vocabulary and
//! `docs/adr/` for the decisions behind the design.
//!
//! v0.3 is embedded and single-threaded in intent. A Node opened with
//! [`Node::open`] is durable through a WAL replayed into an empty
//! engine on every open. Old Versions are never collected. `find` reads
//! candidates from a single-field Index when one fits the filter (see
//! [`Node::create_index`]) and scans the whole Collection otherwise.
//!
//! Start with [`Node`].

mod catalog;
mod collection_path;
mod document;
mod error;
mod field_path;
mod filter;
mod index;
mod keys;
mod mvcc;
mod node;
mod schema;
mod transaction;
pub mod wal;

pub use collection_path::CollectionPath;
pub use document::{Limits, MAX_NAME_BYTES};
pub use error::{Error, Result};
pub use field_path::FieldPath;
pub use filter::{CompareOp, Filter};
pub use index::IndexInfo;
pub use keys::MAX_ID_BYTES;
pub use node::Node;
pub use schema::{FieldSchema, FieldType};
pub use transaction::Transaction;
