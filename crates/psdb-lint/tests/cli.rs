//! The `psdb-lint` binary over a directory tree, as `scripts/lint.sh` runs it.
#![expect(
    clippy::unwrap_used,
    clippy::disallowed_methods,
    reason = "test code: builds a throwaway tree on the real filesystem"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A fresh directory holding `crates/` with the given files.
fn tree(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!("psdb-lint-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    for (path, text) in files {
        let path = root.join("crates").join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    root
}

/// Run `psdb-lint` in `dir` with `args`: exit code, and stdout.
fn lint(dir: &Path, args: &[&str]) -> (Option<i32>, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_psdb-lint"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8(output.stdout).unwrap(),
    )
}

#[test]
fn walks_every_rust_file_under_crates_and_fails_on_a_problem() {
    let root = tree(
        "walk",
        &[
            (
                "a/src/clean.rs",
                "// Retry: the OS may return EINTR.\nfn f() {}\n",
            ),
            (
                "a/src/deep/bad.rs",
                "fn g() {}\n// TODO: tidy\n// let y = 2;\n",
            ),
            ("a/notes.txt", "// TODO: not Rust, so not checked\n"),
        ],
    );
    let (code, out) = lint(&root, &[]);
    fs::remove_dir_all(&root).unwrap();
    // 1, not just failure: a panic fails too.
    assert_eq!(code, Some(1));
    let bad = Path::new("crates/a/src/deep/bad.rs").display().to_string();
    assert_eq!(
        out,
        format!(
            "{bad}:2: TODO-style marker: write `// shortcut: <the limit>, <when to upgrade>` or a ROADMAP.md item\n\
             {bad}:3: commented-out code: delete it, git keeps the history\n"
        )
    );
}

#[test]
fn passes_and_prints_nothing_on_clean_files() {
    let root = tree(
        "clean",
        &[("a/src/lib.rs", "// Why: a reason.\nfn f() {}\n")],
    );
    let (code, out) = lint(&root, &["crates/a/src/lib.rs"]);
    let (code_all, out_all) = lint(&root, &[]);
    fs::remove_dir_all(&root).unwrap();
    assert_eq!((code, out.as_str()), (Some(0), ""));
    assert_eq!((code_all, out_all.as_str()), (Some(0), ""));
}
