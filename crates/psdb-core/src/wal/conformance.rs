//! Conformance suite for [`WalFile`] implementations.
//!
//! Every implementation calls [`run_all`] from its tests with a function that
//! picks a fresh, empty place for a WAL file and returns an opener for it.
//! Each call of the opener opens the file at that place, creating it the
//! first time:
//!
//! ```
//! use psdb_core::wal::{SimDisk, conformance};
//!
//! conformance::run_all(|| {
//!     let disk = SimDisk::new();
//!     move || disk.file()
//! });
//! ```
//!
//! The suite checks the in-process rules of the contract. What survives
//! power loss is checked against [`SimDisk`](super::SimDisk) by the crash
//! tests, since a real file cannot be made to lose power.

use super::WalFile;

/// Run every conformance check, each against a fresh place from `make`.
/// Panics on the first violation.
pub fn run_all<F: WalFile, O: FnMut() -> F>(make: impl Fn() -> O) {
    created_empty(make());
    appends_in_order(make());
    reading_does_not_move_appends(make());
    truncate_then_append(make());
    synced_bytes_survive_reopen(make());
}

fn created_empty<F: WalFile>(mut open: impl FnMut() -> F) {
    assert_eq!(open().read_all().unwrap(), b"", "a new file must be empty");
}

fn appends_in_order<F: WalFile>(mut open: impl FnMut() -> F) {
    let mut file = open();
    file.append(b"ab").unwrap();
    file.append(b"").unwrap();
    file.append(b"cd").unwrap();
    assert_eq!(
        file.read_all().unwrap(),
        b"abcd",
        "appends are readable at once"
    );
}

fn reading_does_not_move_appends<F: WalFile>(mut open: impl FnMut() -> F) {
    let mut file = open();
    file.append(b"ab").unwrap();
    file.read_all().unwrap();
    file.append(b"c").unwrap();
    assert_eq!(
        file.read_all().unwrap(),
        b"abc",
        "append writes at the end after a read"
    );
}

fn truncate_then_append<F: WalFile>(mut open: impl FnMut() -> F) {
    let mut file = open();
    file.append(b"abcdef").unwrap();
    file.truncate(2).unwrap();
    file.append(b"x").unwrap();
    assert_eq!(
        file.read_all().unwrap(),
        b"abx",
        "append writes at the new end"
    );
}

fn synced_bytes_survive_reopen<F: WalFile>(mut open: impl FnMut() -> F) {
    let mut file = open();
    file.append(b"abc").unwrap();
    file.truncate(2).unwrap();
    file.sync().unwrap();
    file.sync_dir().unwrap();
    drop(file);
    assert_eq!(open().read_all().unwrap(), b"ab");
}
