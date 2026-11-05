//! Shared setup for integration tests.

// Each test file uses only some of these.
#![allow(dead_code)]

use psdb_core::wal::SimDisk;
use psdb_core::{CollectionPath, FieldPath, Limits, Node, Result};
use psdb_storage_memory::MemoryEngine;

/// A Node with `shop.orders` already created.
pub fn shop() -> Node<MemoryEngine> {
    let node = Node::new(MemoryEngine::new()).unwrap();
    node.create_namespace("shop").unwrap();
    node.create_collection(&collection("shop.orders")).unwrap();
    node
}

/// A durable Node over `disk`, replaying the WAL already on it.
pub fn open(disk: &SimDisk) -> Result<Node<MemoryEngine>> {
    Node::open_with(MemoryEngine::new(), disk.file(), Limits::default())
}

/// A durable Node over a fresh `disk`, with `shop.orders` already created.
pub fn durable_shop(disk: &SimDisk) -> Node<MemoryEngine> {
    let node = open(disk).unwrap();
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
