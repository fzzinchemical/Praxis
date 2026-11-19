## 14. Testing

| Layer | What | How |
|---|---|---|
| Unit | Parsers, budgets, rules, recovery, config merge, hook matching | Plain `#[test]`, no I/O |
| Integration | Agent loop, provider, tools, sessions | `wiremock` fake server with fixtures in `crates/praxis-core/tests/fixtures/lmstudio/` and `.../anthropic/` (T8.1); temp directories |
| Security | §10 invariants | `crates/praxis-core/tests/security.rs` plus hostile fixture repo |
| Protocol | ACP | In-process via the crate's `Channel` transport; mock client |
| MCP | Client | Small test server built with `rmcp`'s server features as a dev-dependency |
| Plugin compat | Real format | Fixture plugin with a skill, a hook and an MCP server |
| Extension | Logic | Native tests on protocol and message-mapping code; **manual checklist** in `editors/vscode/CHECKLIST.md` for UI |
| Evals | Real model quality | `cargo xtask eval`, outside verify |

**Fixtures** are recorded from real servers by T0.3 and stored as-is, including malformed output. Never hand-edit them to make a test pass; if behaviour changes, re-record.

`scripts/verify` runs `cargo fmt --all --check`, then `cargo clippy` and the native tests on the workspace excluding wasm crates, run through `cargo llvm-cov` with line-coverage floors in `scripts/coverage-floors`, then `cargo clippy` (which includes the check) for wasm crates with `--target wasm32-unknown-unknown`. Add `cargo audit` if the human approves the tool; it isn't installed.

### Evals

Each `evals/<task>/` has `task.toml` (prompt, limits), `repo/` (fixture) and `check.sh` (exit 0 means success, written so it can't be satisfied by editing the tests: tests live outside the agent's writable path, or are restored before checking). Run with sandbox on and `auto-edit` mode. Until T3.3 provides both, `cargo xtask eval` drives `praxis-core` directly, approves every tool call and instead runs each task inside `bwrap`: only the task's copy is writable, the home directory isn't mounted, and the only network path is a forwarded socket to LM Studio. `check.sh` runs in a second `bwrap` with no network, because it executes agent-written code. The command fails if `bwrap` is missing.

`check.sh [workdir]` judges a copy of the agent's edited repo and never changes it. Each task also has `hidden/` (tests outside the writable path), `solution.patch` and optionally `bad-solutions/*.patch`. `scripts/prove-evals` shows that every check fails on the untouched fixture and on tampered tests, passes on the solution and fails on each bad solution; it needs no model and stays out of verify because it compiles every fixture repeatedly.

Scoreboard per model: success rate, turns, wall time, tool-call parse failures, recoveries, loop stops, prompt tokens processed versus cached, re-fetches of masked output, and injection eval outcomes. Token counts alone overstate savings, so every context feature is compared with it switched off. Written as JSON and committed to `evals/results/` when a milestone closes. A metric the praxis can't produce yet is `null`, with its reason under `null_reasons`. Model runs vary, so run each task at least 3 times with a fixed seed where supported. `--tasks <glob>[,<glob>]` runs only matching task directories, and `tag:<name>` matches the `tags` list in `task.toml`; the board records the filter as `tasks_filter` (T14.5). Betas ship with targeted runs or none; the full 3-run board on both chosen models is the 1.0 gate (T14.6).

