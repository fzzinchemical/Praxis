# Praxis

**A local-first coding agent, written in Rust, built to make small local models useful for real coding work.**

Praxis runs an agent loop against models served by [LM Studio](https://lmstudio.ai) on your own machine. It reads and edits files, runs commands in a sandbox, talks to MCP servers, loads Claude Code-style plugins, skills and `AGENTS.md`/`CLAUDE.md` memory, and works from the terminal or inside VS Code.

> [!NOTE]
> **Beta 1 (`v1.0.0-beta.1`).** Linux x86_64 only. Expect rough edges and breaking changes. Upgrading from a build named `harness`: rename `~/.config/harness`, `~/.local/share/harness` and each project's `.harness` to `praxis` (`.praxis`), and `HARNESS_*` variables to `PRAXIS_*`.

**Contents:** [Why local models](#why-local-models-first) · [What it does](#what-it-does) · [Install](#install) · [Quick start](#quick-start) · [Configuration](#configuration) · [VS Code](#vs-code) · [Status](#status-and-limits) · [Development](#development) · [Docs](#further-reading)

---

## Why local models first

Most coding agents are built for large hosted models and treat local models as an afterthought. Praxis is built the other way around, because local models are the ones that need help:

- **Your code stays on your machine.** Nothing is sent to a third party unless you configure it to. There is no telemetry. Proprietary code, credentials in your working tree and unreleased work never leave the box.
- **No per-token bill, no rate limits, no outage you don't control.** Once you have the hardware, an agent that runs hundreds of tool calls costs electricity, not API spend. It also works offline.
- **You pick and pin the model.** A hosted model can change behind the same name. A local model file doesn't, so results stay reproducible.
- **Small models fail in specific, fixable ways,** and a harness can fix them in code. Generic agents don't focus on these failures:

| Problem with local models | What Praxis does |
|---|---|
| Small context windows, silently truncated by the server | Reads the model's real loaded context, keeps an explicit token budget and detects overflow (LM Studio reports it as HTTP 200 plus an SSE error) |
| Tool calls that come back malformed or as plain text | Per-model parsers, JSON repair, schema validation, and a constrained retry with the error sent back to the model |
| Slow prompt reprocessing when the prefix changes | Append-only prompt layout, so LM Studio's prefix cache keeps working |
| Many MCP tool schemas filling the context | Indexed tool mode: the model sees a compact index and fetches schemas when it needs them |
| Models that loop on the same failing command | Loop detection that stops and redirects the model |
| Large, noisy tool output | Output filters and masking of old tool results |
| Quality varies widely by model | Per-model profiles plus an eval suite (`evals/`) that measures each model |

On the project's eval suite, with `qwen3.5-4b`, Praxis averaged about 31 s per task against 144 s for OpenCode on the same model, mostly because loop detection stops runaway retries. Pass rates were 6/13 for Praxis and 8/13 for OpenCode. With `google/gemma-4-12b-qat` both passed 10/13. The method and per-task results are in [docs/baseline.md](docs/baseline.md).

Hosted models still help with tasks local models can't do yet. Support for the Anthropic API as an **opt-in** backend is designed ([ADR 0004](docs/adr/0004-anthropic-backend-and-per-agent-models.md)): it must be enabled in your user config, a project can never turn it on, and each agent can use its own model. Its configuration and safety checks exist, but the Anthropic chat client is **not in Beta 1** yet.

## What it does

- **Agent loop** with streaming output, built-in `Read`, `Write`, `Edit`, `Grep`, `Glob` and `Bash` tools, and a turn summary that lists the files changed and whether tests actually ran.
- **Safety in code, not in the prompt.** Permission modes (`read-only`, `ask`, `auto-edit`) with allow and deny rules, workspace confinement, a Landlock sandbox for `Bash`, hard denials for secrets, and a trust gate for project config. Model output, file contents and tool results are treated as untrusted.
- **Sessions:** resume (`--resume`), checkpoints and `/undo`, and plan-first mode (`/plan`, then `/execute`).
- **MCP client** over stdio and streamable HTTP, read from config or `.mcp.json`. Tool definitions are pinned, and a changed tool needs your approval.
- **Claude Code compatibility:** plugins (`plugin.json`, skills, commands, agents, hooks, MCP), standalone skills and agents in `.claude/` and `~/.claude/`, `CLAUDE.md` and `AGENTS.md` memory with imports and nested files, path-scoped rules, and Claude Code settings.
- **Editors:** a VS Code extension (Rust compiled to WebAssembly, no npm) with chat, diff review, sessions and plugin views. Any [Agent Client Protocol](https://agentclientprotocol.com) client, such as Zed, can use `praxis acp`.

## Install

### From a release

Download from the [Releases page](https://github.com/fzzinchemical/Praxis/releases):

- `praxis-<version>-x86_64-linux.tar.gz`: the CLI
- `praxis-<version>.vsix`: the VS Code extension

```sh
tar xzf praxis-*-x86_64-linux.tar.gz
install -Dm755 praxis ~/.local/bin/praxis
```

### From source

Requires Rust 1.99 or newer.

```sh
git clone https://github.com/fzzinchemical/Praxis.git
cd Praxis
cargo build --release -p praxis-cli    # binary at target/release/praxis
```

### Requirements

- Linux 6.7 or newer with Landlock enabled (Landlock ABI 4, needed to block network access). The `Bash` sandbox refuses to run rather than run unsandboxed. You can turn it off with `[sandbox] mode = "off"`, at your own risk.
- [LM Studio](https://lmstudio.ai) with its local server running (by default on port 1234) and at least one model that supports tool calling. We test with `qwen3.5-4b` and `google/gemma-4-12b-qat`. `qwen2.5-coder-7b-instruct` does **not** work: it never emits tool calls.

## Quick start

```sh
# 1. Start LM Studio's server and load a model, then check Praxis can see it:
praxis models

# 2. One-shot task in the current directory:
praxis "fix the failing test in src/parse.rs"

# 3. Or start an interactive session:
praxis
```

In the REPL, use `/plan <task>` to plan in read-only mode, `/execute` to carry out the plan, `/undo` to revert the last turn's edits and `/exit` to quit. Ctrl-C cancels the current turn; press it again to exit.

Other commands:

```sh
praxis --model qwen3.5-4b "..."    # choose the model (default: the first one loaded)
praxis --resume                    # continue the newest session in this workspace
praxis sessions                    # list saved sessions
praxis plugin install <git-url>    # plugins start disabled; enable them after review
praxis plugin list
praxis acp                         # Agent Client Protocol over stdio, for editors
```

Only a person at a terminal can approve a permission prompt. Piped or non-interactive runs deny anything that needs approval.

## Configuration

Config is TOML, layered from lowest to highest priority: built-in defaults, then `~/.config/praxis/config.toml` (user), then `.praxis/config.toml` (project; untrusted until you trust the workspace), then `PRAXIS_*` environment variables, then `--config-override KEY=VALUE` flags. Unknown keys are errors.

```toml
default_model = "qwen3.5-4b"

[lmstudio]
base_url = "http://localhost:1234"
context = 32768               # context size to load models with

[agent]
auto_load = false             # load the model without asking

[permissions]
mode = "ask"                  # read-only | ask | auto-edit
allow = ["Bash(cargo test*)"]
deny = ["Read(**/.env*)"]

[sandbox]
mode = "on"
network = false

[mcp.filesystem]
command = "mcp-server-filesystem"
args = ["."]
```

The full reference is [docs/SPEC.md §8](docs/SPEC.md#8-config-trust-and-sessions).

## VS Code

1. Install the extension: `code --install-extension praxis-<version>.vsix`.
2. Set **Praxis: Binary Path** (`praxis.binaryPath`) to your `praxis` binary if it isn't on `PATH`.
3. Run **Praxis: Open Chat** from the command palette.

## Status and limits

Beta 1 covers milestones M0 to M6 and most of M9 (see [docs/TASKS.md](docs/TASKS.md)). Not yet done:

- Anthropic chat backend (M8): configuration only.
- Webview chat polish, change-set review and Zed packaging (M5).
- Performance targets measured and a written threat model (M7).
- Sandboxing on macOS and Windows: not planned.
- Hooks and MCP servers run outside the sandbox. Only enable plugins you trust.

## Development

```sh
git config core.hooksPath .githooks   # run verify before every commit
./scripts/verify                      # format, clippy, all tests
cargo xtask vsix                      # build the VS Code extension (needs wasm-bindgen CLI 0.2.128)
```

Tests never need a real model: they use a fake server and recorded fixtures. Model evals (`cargo xtask eval --model <id>`) need a running LM Studio. Contributor rules are in [AGENTS.md](AGENTS.md).

All product code is Rust ([ADR 0001](docs/adr/0001-rust-only-with-minimal-vscode-glue.md)).

### Releases

CI runs `./scripts/verify` on every push and pull request to `main`. Pushing a version tag that points to a commit on `main` builds the Linux binary and the `.vsix` and publishes a GitHub release. A tag with a suffix (`v1.0.0-beta.1`) becomes a pre-release.

```sh
git tag v1.0.0-beta.1 && git push origin v1.0.0-beta.1
```

## Further reading

- [ROADMAP.md](ROADMAP.md): why the project exists, architecture and milestones
- [docs/SPEC.md](docs/SPEC.md): the implementation contract
- [docs/adr/](docs/adr/): design decisions
- [docs/RESEARCH.md](docs/RESEARCH.md): the evidence behind them
