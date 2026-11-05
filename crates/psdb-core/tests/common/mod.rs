//! Shared setup for integration tests.

use psdb_core::Node;
use psdb_storage_memory::MemoryEngine;

/// A Node with `shop.orders` already created.
pub fn shop() -> Node<MemoryEngine> {
    let node = Node::new(MemoryEngine::new());
    node.create_namespace("shop").unwrap();
    node.create_collection("shop", "orders").unwrap();
    node
}
