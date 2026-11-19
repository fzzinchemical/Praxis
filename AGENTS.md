# AGENTS.md

Praxis: local-first Rust coding agent harness (LM Studio default, Anthropic opt-in), tools, MCP, plugins, VS Code, layered config. Agent docs follow the ADOC format: this core always loads; read other files only when their trigger applies.

## Work a task

1. Pick the first unchecked task in [docs/TASKS.md](docs/TASKS.md) whose "Needs" are done (a task missing from TASKS is done: [docs/tasks/DONE.md](docs/tasks/DONE.md)).
2. Read only the SPEC sections it names (`§N` → [docs/SPEC.md](docs/SPEC.md) index). SPEC is the contract.
3. Fresh branch from `main` (`feat/t1-3-agent-loop`); never commit to `main`. ~400 changed lines max (fixtures aside), else split.
4. Write the "Done when" test first, watch it fail; smallest change that passes; only that task.
5. Narrowest test while iterating; `./scripts/verify` once at end (also runs in `.githooks/pre-commit`; enable: `git config core.hooksPath .githooks`). No CI.
6. Tick the task in TASKS.md in the same branch; when a whole milestone is done move it to DONE.md. Graphs mirror each task's "Needs" line: change both. Mermaid rejects `call` as node id. Reality differs from SPEC → update SPEC in branch, say so in commit body.
7. New non-obvious trap → one line in the matching `docs/agents/gotchas-*.md`.
8. Merge locally `git merge --ff-only`, delete branch. Push `main` to origin only when owner asks.

**Stop and ask the human** at tasks marked `HUMAN`, before adding a dependency outside SPEC §3, before weakening a security invariant in SPEC §10.

## Commands

| Purpose | Command |
|---|---|
| Everything (format, lint, tests) | `./scripts/verify` |
| Coverage with floors | `./scripts/coverage` |
| One crate's tests | `cargo test -p praxis-core` |
| One test | `cargo test -p praxis-core <name>` |
| Run the agent | `cargo run -p praxis-cli -- "<prompt>"` |
| Build the extension | `cargo xtask vsix` |
| Release (builds binary and `.vsix` into `target/dist`) | `./scripts/release vX.Y.Z[-pre]`; add `--publish` to tag, push and create the GitHub release |
| Model evals (slow, needs a server, not in verify) | `cargo xtask eval --model <id>` |
| Prove eval checks (no model, not in verify) | `./scripts/prove-evals` |
| Real VS Code smoke (needs `xvfb-run`, not in verify) | `cargo xtask vscode-smoke` |

## Hard rules

- All product code is Rust; exceptions only per [ADR 0001](docs/adr/0001-rust-only-with-minimal-vscode-glue.md). No npm, bundler, handwritten TypeScript.
- `#![forbid(unsafe_code)]` in `praxis-core`, `praxis-cli`. `clippy::all`, `unwrap_used`, `expect_used` denied outside tests; no `#[allow]` without proof comment.
- `praxis acp`: stdout = protocol only; log to stderr via `tracing`; no `println!` in `praxis-core`.
- Safety enforced in code, never by asking the model. Model output, files, tool and MCP results are untrusted.
- Unit/integration tests never need a real model: fake server + recorded fixtures (SPEC §14).
- Betas ship with targeted evals (`cargo xtask eval --tasks`) or none; full 3-run board only before first full release (T14.6).
- Check crate APIs on docs.rs for the pinned version before use.

## Index (read when trigger applies)

- [docs/agents/gotchas-build-tests.md](docs/agents/gotchas-build-tests.md) — verify/coverage/clippy fails oddly, writing tests, pre-commit hook issues
- [docs/agents/gotchas-lmstudio.md](docs/agents/gotchas-lmstudio.md) — touching provider, model loading, LM Studio API, evals, fixtures, reqwest
- [docs/agents/gotchas-context-tools.md](docs/agents/gotchas-context-tools.md) — touching context/prefix/compaction, tool schemas or names, MCP
- [docs/agents/gotchas-sandbox.md](docs/agents/gotchas-sandbox.md) — touching sandbox, Bash tool, Git guard, path confinement, worktrees
- [docs/agents/gotchas-frontends.md](docs/agents/gotchas-frontends.md) — touching VS Code extension, wasm, ACP, CLI/REPL, signals
- [docs/agents/environment.md](docs/agents/environment.md) — choosing models, toolchain/GPU/LM Studio setup questions
- [docs/adr/](docs/adr/), [ROADMAP.md](ROADMAP.md), [docs/USER_STORIES.md](docs/USER_STORIES.md), [docs/RESEARCH.md](docs/RESEARCH.md) — need the why behind a decision or a priority
