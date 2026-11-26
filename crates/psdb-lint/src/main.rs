//! `psdb-lint`: the comment rules in `docs/CONVENTIONS.md` that clippy
//! cannot check. Checks the given files, or every `.rs` file under
//! `crates/` if none are given, and prints one line per problem.
#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a command-line tool reports on the terminal"
)]
#![expect(
    clippy::disallowed_methods,
    reason = "a development tool reading source files, outside the simulated system"
)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::{env, fs, io};

use syn::{Block, Expr, Stmt};

/// The longest run of comment lines tried as one piece of code. Longer
/// commented-out code is still caught through its shorter runs, and the cap
/// keeps the check linear in the length of a comment.
const MAX_RUN: usize = 8;

fn main() -> ExitCode {
    let args: Vec<PathBuf> = env::args_os().skip(1).map(PathBuf::from).collect();
    let paths = if args.is_empty() {
        rust_files(Path::new("crates"))
    } else {
        Ok(args)
    };
    let result = paths.and_then(|paths| {
        let mut clean = true;
        for path in paths {
            for problem in check(&fs::read_to_string(&path)?) {
                println!("{}:{}: {}", path.display(), problem.line, problem.message);
                clean = false;
            }
        }
        Ok(clean)
    });
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("psdb-lint: {e}");
            ExitCode::from(2)
        }
    }
}

/// Every `.rs` file under `dir`.
fn rust_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            files.extend(rust_files(&path)?);
        } else if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
    Ok(files)
}

#[derive(Debug, PartialEq)]
struct Problem {
    /// 1-based.
    line: usize,
    message: &'static str,
}

const MARKER: &str =
    "TODO-style marker: write `// shortcut: <the limit>, <when to upgrade>` or a ROADMAP.md item";
const CODE: &str = "commented-out code: delete it, git keeps the history";

/// The problems in one file's `source`, in line order.
fn check(source: &str) -> Vec<Problem> {
    let mut problems = Vec::new();
    // Consecutive plain `//` comment lines: (line number, text).
    let mut run: Vec<(usize, &str)> = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let number = index + 1;
        let comment = line.trim_start().strip_prefix("//");
        if comment.is_some_and(has_marker) {
            problems.push(Problem {
                line: number,
                message: MARKER,
            });
        }
        if let Some(text) = comment.filter(|c| !c.starts_with('/') && !c.starts_with('!')) {
            run.push((number, text.trim()));
        } else {
            problems.extend(code_lines(&run).map(|line| Problem {
                line,
                message: CODE,
            }));
            run.clear();
        }
    }
    problems.extend(code_lines(&run).map(|line| Problem {
        line,
        message: CODE,
    }));
    problems.sort_by_key(|p| p.line);
    problems
}

fn has_marker(comment: &str) -> bool {
    comment
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| matches!(word, "TODO" | "FIXME" | "XXX" | "HACK"))
}

/// Line numbers of `comments` (consecutive comment lines) that lie in a run
/// of at most [`MAX_RUN`] lines parsing as code.
fn code_lines(comments: &[(usize, &str)]) -> impl Iterator<Item = usize> {
    let mut hit = vec![false; comments.len()];
    for start in 0..comments.len() {
        for end in start..comments.len().min(start + MAX_RUN) {
            let text: Vec<&str> = comments[start..=end].iter().map(|&(_, t)| t).collect();
            if is_code(&text.join("\n")) {
                hit[start..=end].fill(true);
            }
        }
    }
    comments
        .iter()
        .zip(hit)
        .filter_map(|(&(line, _), hit)| hit.then_some(line))
        .collect::<Vec<_>>()
        .into_iter()
}

/// Whether `text` parses as Rust that is more than a bare word, literal or
/// arithmetic on words, trying the ways a fragment is usually cut out of
/// code: missing its `;`, its receiver, its enclosing `match` or a brace.
fn is_code(text: &str) -> bool {
    // rustfmt never ends a line with a lone `.`; prose ends sentences so.
    let ends_sentence = |line: &str| line.ends_with('.') && !line.ends_with("..");
    if !text.contains(|c| ";(){}[]=.!:".contains(c)) || text.lines().any(ends_sentence) {
        return false;
    }
    [
        text.to_owned(),
        format!("{text};"),
        format!("{text} }}"),
        format!("{{ {text}"),
        format!("x{text}"),
        format!("x{text};"),
        format!("match x {{ {text} }}"),
    ]
    .iter()
    .any(|candidate| {
        syn::parse_str::<Block>(&format!("{{ {candidate} }}")).is_ok_and(|block| {
            block
                .stmts
                .iter()
                .any(|s| !matches!(s, Stmt::Expr(Expr::Path(_) | Expr::Lit(_), None)))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flagged(source: &str) -> Vec<usize> {
        check(source).iter().map(|p| p.line).collect()
    }

    #[test]
    fn finds_commented_out_code_in_its_usual_shapes() {
        for code in [
            "// let x = node.engine.scan(range);",
            "// batch.delete(key.to_vec())",
            "// self.done = true",
            "// .map(|r| r.len())",
            "// }",
            "// if done {",
            "// assert_eq!(a, b);",
            "// Some(x) => return x,",
            "// x += 1",
        ] {
            assert_eq!(flagged(code), [1], "{code}");
        }
        let split = "// let total = items\n//     .iter()\n//     .sum::<u64>();";
        assert_eq!(flagged(split), [1, 2, 3]);
    }

    #[test]
    fn leaves_prose_alone() {
        for prose in [
            "// The recheck drops the extras (see filter.rs);",
            "// Retry, since the OS may return EINTR (see read(2));",
            "// Close the batch only between keys: deleting a Tombstone\n// without the Versions under it would show them to readers.",
            "// `whole` is checked to be in the i64 range",
            "// Ok",
            "// 42",
            "// a - b",
            "// Done.\n// a - b",
            "/// let x = 1; in a doc comment is an example, not dead code",
            "/// let x = 1;",
            "//! let x = 1;",
            "// shortcut: a hint, not a guarantee; use a fair lock (ADR 0013).",
        ] {
            assert_eq!(flagged(prose), [0; 0], "{prose}");
        }
    }

    #[test]
    fn finds_markers_in_any_comment() {
        assert_eq!(
            flagged("// TODO: speed this up\n/// FIXME\nlet todo = 1;"),
            [1, 2]
        );
        assert_eq!(flagged("// TODOs are tracked in the roadmap"), [0; 0]);
    }
}
