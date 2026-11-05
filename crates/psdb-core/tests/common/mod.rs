//! Shared setup for integration tests.

// Each test file uses only some of these.
#![allow(dead_code)]

use psdb_core::{CollectionPath, FieldPath, Node};
use psdb_storage_memory::MemoryEngine;

/// A Node with `shop.orders` already created.
pub fn shop() -> Node<MemoryEngine> {
    let node = Node::new(MemoryEngine::new());
    node.create_namespace("shop").unwrap();
    node.create_collection(&collection("shop.orders")).unwrap();
    node
}

/// The Collection path `path`, such as `shop.orders`.
pub fn collection(path: &str) -> CollectionPath {
    path.parse().unwrap()
}

/// The Field path `path`, such as `address.city`.
pub fn field_path(path: &str) -> FieldPath {
    path.parse().unwrap()
}
