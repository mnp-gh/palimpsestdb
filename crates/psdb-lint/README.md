# psdb-lint

The comment rules in `docs/CONVENTIONS.md` that clippy cannot check: no `TODO`-style markers, and no commented-out code. It finds commented-out code by parsing comments as Rust with `syn`, so prose that merely looks like code passes. `scripts/lint.sh` runs it on every file; the Claude Code hook runs it on the edited file only.

**Depends on:** no `psdb` crate. **Depended on by:** nothing; it is a development tool.
