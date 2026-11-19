# Gotchas: Sandbox, Git guard, filesystem

- The Git guard runs `Bash` under bwrap only when `.git` is a folder, so a worktree's `.git` file skips it; a test that needs the guard must make a real repo with `git init` (`tests/sandbox.rs`).
- `fs::canonicalize` fails on a dangling symlink, so a naive "deepest existing ancestor" treats it as a new file inside the workspace. Check `symlink_metadata` at each level (`tools/confine.rs`).
- Any binary that runs `Bash` must handle `sandbox-exec` before starting tokio, as `praxis` and `xtask` do, and pass itself as the sandbox helper.
- A worktree's `.git` is a file, so the bwrap Git guard never covers it; this repo's agent worktrees are unguarded (SPEC §9).
- Bash fails when `bwrap` is on PATH but can't create namespaces: `sandbox::helper_command` has no unguarded fallback.
