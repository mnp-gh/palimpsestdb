//! Shared setup for integration tests.

use psdb_core::Database;
use psdb_storage_memory::MemoryEngine;

/// A Database with `shop.orders` already created.
pub fn shop() -> Database<MemoryEngine> {
    let db = Database::new(MemoryEngine::new());
    db.create_database("shop").unwrap();
    db.create_collection("shop", "orders").unwrap();
    db
}
