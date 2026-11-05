//! The PalimpsestDB Database Layer.
//!
//! Everything above the Storage Engine that gives bytes meaning: Documents,
//! the Catalog of Namespaces and Collections, Versions, and Transactions
//! under Snapshot Isolation. See `GLOSSARY.md` for the vocabulary and
//! `docs/adr/` for the decisions behind the design.
//!
//! v0.2 is embedded and single-threaded in intent. A Node opened with
//! [`Node::open`] is durable through a WAL replayed into an empty
//! engine on every open. Old Versions are never collected, and `find` scans
//! the whole Collection.
//!
//! Start with [`Node`].

mod catalog;
mod document;
mod error;
mod filter;
mod keys;
mod mvcc;
mod node;
mod transaction;
pub mod wal;

pub use document::{Limits, MAX_NAME_BYTES};
pub use error::{Error, Result};
pub use filter::{CompareOp, Filter};
pub use keys::MAX_ID_BYTES;
pub use node::Node;
pub use transaction::Transaction;
