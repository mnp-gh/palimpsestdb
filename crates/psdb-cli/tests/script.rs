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
    // Each command and what it prints; most print nothing.
    let steps = [
        ("create-ns shop", ""),
        ("namespaces", "shop\n"),
        ("create-coll shop.orders", ""),
        ("colls shop", "orders\n"),
        ("set-type shop.orders n int strict", ""),
        ("set-type shop.orders m int", ""),
        ("schema shop.orders", "m: int\nn: int strict\n"),
        ("create-index shop.orders n unique", ""),
        ("create-index shop.orders m", ""),
        ("indexes shop.orders", "n unique\nm\n"),
        ("drop-index shop.orders m", ""),
        ("unset-type shop.orders m", ""),
        ("schema shop.orders", "n: int strict\n"),
        ("indexes shop.orders", "n unique\n"),
        (r#"insert shop.orders {"_id": 1, "n": 5}"#, "1\n"),
        (r#"insert shop.orders {"_id": 2, "n": 7}"#, "2\n"),
        ("get shop.orders 1", "{\"_id\":1,\"n\":5}\n"),
        (r#"replace shop.orders 2 {"n": 8}"#, "replaced\n"),
        ("delete shop.orders 1", "deleted\n"),
        ("get shop.orders 1", "not found\n"),
        (
            r#"find shop.orders {"n": {"$gt": 6}}"#,
            "{\"_id\":2,\"n\":8}\n(1 found)\n",
        ),
        ("find shop.orders", "{\"_id\":2,\"n\":8}\n(1 found)\n"),
    ];
    let commands: Vec<&str> = steps.iter().map(|(command, _)| *command).collect();
    let script = commands.join("\n") + "\n";
    let expected: String = steps.iter().map(|(_, output)| *output).collect();
    assert_eq!(run(&script), (true, expected));
}

#[test]
fn a_script_stops_at_its_first_failed_step() {
    let script = "create-ns shop\ncreate-ns shop\nnamespaces\n";
    assert_eq!(run(script), (false, String::new()));
}
