# Gotchas: Build, verify, coverage, test harness

- The pre-commit hook checks the working tree, not just the staged files, so unstaged changes can fail or pass a commit.
- `scripts/verify` runs tests through `cargo llvm-cov` (pacman `cargo-llvm-cov`); code that runs only inside `sandbox-exec` shows as uncovered because the sandboxed child can't write its profile.
- `--workspace` overrides `default-members`, so it pulls wasm crates into native builds. Use `--exclude` as `scripts/verify` does.
- Clippy's `allow-unwrap-in-tests` skips helper functions in `tests/*.rs` unless the file starts with `#![cfg(test)]`.
- The spool directory is deleted when its `ToolContext` drops, so a test that reads a spooled path must keep the context alive (`tests/tools.rs`).
- CLI tests set `XDG_CONFIG_HOME` and `HOME` to their temp dir, or the developer's own `~/.config/praxis/config.toml` and `~/.claude/skills` change their results.
- `praxis = false` test targets build without `cfg(test)`, so `#![cfg(test)]` removes `main` and clippy's test allowances don't apply (`tests/mcp.rs`).
- The pre-commit hook exports `GIT_DIR` and `GIT_INDEX_FILE`, so any `git` a test spawns must `env_remove` them (and `GIT_WORK_TREE`), or it writes to this repo's index and config (`tests/plugin_cli.rs`).
- Stale objects in a checkout's `llvm-cov` target can count a source file twice and drop coverage below the floors. If one checkout fails where another passes, run `cargo llvm-cov clean --workspace` before blaming the code.
