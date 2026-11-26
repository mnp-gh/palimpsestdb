//! Shared setup for integration tests.
#![expect(clippy::unwrap_used, reason = "test code: a failure should panic")]
#![allow(dead_code, reason = "each test file uses only some of these")]

use psdb_core::wal::SimDisk;
use psdb_core::{CollectionPath, FieldPath, IdGenerator, Limits, Node, Result};
use psdb_storage_memory::MemoryEngine;

/// A Node with `shop.orders` already created.
pub(crate) fn shop() -> Node<MemoryEngine> {
    let node = Node::new(MemoryEngine::new()).unwrap();
    node.create_namespace("shop").unwrap();
    node.create_collection(&collection("shop.orders")).unwrap();
    node
}

/// A durable Node over `disk`, replaying the WAL already on it.
pub(crate) fn open(disk: &SimDisk) -> Result<Node<MemoryEngine>> {
    Node::open_with(
        MemoryEngine::new(),
        disk.file(),
        Limits::default(),
        IdGenerator::default(),
    )
}

/// A durable Node over a fresh `disk`, with `shop.orders` already created.
pub(crate) fn durable_shop(disk: &SimDisk) -> Node<MemoryEngine> {
    let node = open(disk).unwrap();
    node.create_namespace("shop").unwrap();
    node.create_collection(&collection("shop.orders")).unwrap();
    node
}

/// The Collection path `path`, such as `shop.orders`.
pub(crate) fn collection(path: &str) -> CollectionPath {
    path.parse().unwrap()
}

/// The Field path `path`, such as `address.city`.
pub(crate) fn field_path(path: &str) -> FieldPath {
    path.parse().unwrap()
}
