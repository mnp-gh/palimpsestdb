//! The PalimpsestDB Document Layer.
//!
//! Everything above the Storage Engine that gives bytes meaning: Documents,
//! the Catalog of Databases and Collections, Versions, and Transactions
//! under Snapshot Isolation. See `GLOSSARY.md` for the vocabulary and
//! `docs/adr/` for the decisions behind the design.
//!
//! v0.3 is embedded and single-threaded in intent. A Database opened with
//! [`Database::open`] is durable through a WAL replayed into an empty
//! engine on every open. Old Versions are never collected. `find` reads
//! candidates from a single-field Index when one fits the filter (see
//! [`Database::create_index`]) and scans the whole Collection otherwise.
//!
//! Start with [`Database`].

mod catalog;
mod database;
mod document;
mod error;
mod filter;
mod index;
mod keys;
mod mvcc;
mod schema;
mod transaction;
pub mod wal;

pub use database::Database;
pub use document::{Limits, MAX_NAME_BYTES};
pub use error::{Error, Result};
pub use filter::{CompareOp, Filter};
pub use index::IndexInfo;
pub use keys::MAX_ID_BYTES;
pub use schema::{FieldSchema, FieldType};
pub use transaction::Transaction;
