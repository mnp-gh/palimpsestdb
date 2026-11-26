//! The `psdb` binary run on a script through stdin, as a user pipes one in.
#![expect(clippy::unwrap_used, reason = "test code: a failure should panic")]

use std::io::Write;
use std::process::{Command, Stdio};

/// Run `script` through `psdb` and return its exit success and stdout.
fn run(script: &str) -> (bool, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_psdb"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    (
        output.status.success(),
        String::from_utf8(output.stdout).unwrap(),
    )
}

#[test]
fn every_command_does_what_it_prints() {
    let script = r#"create-ns shop
namespaces
create-coll shop.orders
colls shop
set-type shop.orders n int strict
set-type shop.orders m int
schema shop.orders
create-index shop.orders n unique
create-index shop.orders m
indexes shop.orders
drop-index shop.orders m
unset-type shop.orders m
schema shop.orders
indexes shop.orders
insert shop.orders {"_id": 1, "n": 5}
insert shop.orders {"_id": 2, "n": 7}
get shop.orders 1
replace shop.orders 2 {"n": 8}
delete shop.orders 1
get shop.orders 1
find shop.orders {"n": {"$gt": 6}}
find shop.orders
"#;
    let expected = r#"shop
orders
m: int
n: int strict
n unique
m
n: int strict
n unique
1
2
{"_id":1,"n":5}
replaced
deleted
not found
{"_id":2,"n":8}
(1 found)
{"_id":2,"n":8}
(1 found)
"#;
    assert_eq!(run(script), (true, expected.to_owned()));
}

#[test]
fn a_script_stops_at_its_first_failed_step() {
    let script = "create-ns shop\ncreate-ns shop\nnamespaces\n";
    assert_eq!(run(script), (false, String::new()));
}
