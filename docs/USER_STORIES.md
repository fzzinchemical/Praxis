# User stories and MoSCoW priorities

What the first release (M7) has to do for its users, and what can be cut if time runs short. Build order is in [TASKS.md](TASKS.md), and the design is in [SPEC.md](SPEC.md).

> [!IMPORTANT]
> **Must is too large right now:** 37 of 53 feature tasks (70%), against the usual MoSCoW guideline of about 60%. The reason is that VS Code, MCP and plugins were all requested as core features. See [Is this scope realistic?](#is-this-scope-realistic) for the decision this needs.

**Contents:** [Priorities](#priorities) · [Who the stories are for](#who-the-stories-are-for) · [Feature map](#feature-map) · [Must](#must-have) · [Should](#should-have) · [Could](#could-have) · [Won't](#wont-have-this-release) · [Enabling work](#enabling-work) · [Scope check](#is-this-scope-realistic)

---

## Priorities

| Priority | Meaning | Test |
|---|---|---|
| **Must** | The release is unusable, unsafe or misses its purpose without it | Would we delay the release for this? Yes |
| **Should** | Important, with a workable alternative for now | Painful to leave out, but the release still works |
| **Could** | Nice to have, and the first thing to cut | Few users would notice its absence |
| **Won't** | Deliberately not in this release | Agreed out of scope, so nobody builds it by accident |

## Who the stories are for

| Persona | Who they are |
|---|---|
| **Developer** | Runs coding models in LM Studio on their own machine and wants a capable agent with nothing leaving it |
| **VS Code user** | The same developer, working from the editor instead of the terminal |
| **Plugin user** | Wants to reuse existing skills, hooks and MCP servers without rewriting them |
| **Hybrid user** | The same developer, who opts in to a Claude model for work a local model can't do |
| **Maintainer** | Builds the praxis and needs evidence that it works |

---

## Feature map

| Feature area | Must | Should | Could | Won't |
|---|---|---|---|---|
| **LM Studio and agent core** | US-01 to US-04 | US-48 honest agent behaviour | US-38 rich TUI | Other backends, hosting models |
| **Local model reliability** | US-05 to US-10 | US-30 loop detection, US-31 repo map, US-45 repeat reads, US-46 output filters, US-47 constrained retry | | RAG index, model routing, helper models, prompt compression, minification |
| **Safety** | US-11 to US-15 | US-33 undo | | Windows and macOS sandboxing |
| **Config and sessions** | US-16, US-17 | US-32 `AGENTS.md`, US-57 memory files, US-58 nested memory, US-59 standalone skills and agents, US-60 Claude Code settings | | Telemetry |
| **MCP** | US-18 to US-20, US-44 tool pinning | US-34 remote servers | US-39 resources and prompts | OAuth |
| **VS Code** | US-21 to US-23 | US-35 editor context, US-49 turn summary, US-50 change-set review, US-51 plan first | US-40 Zed, US-41 diagnostics, US-43 VSCodium | Marketplace publishing, proactive suggestions, automatic LLM review |
| **Plugins** | US-24 to US-28 | US-36 agents, US-37 plugin settings | US-42 plugin `bin/` | Marketplace, JS workflows and mods, other components |
| **Performance and evidence** | US-29 | | US-52 timed trial | |
| **Acceptance tests (M12)** | | US-61 REPL commands, US-62 sessions, US-63 stop reasons, US-64 MCP failures | | |
| **Anthropic and per-agent models** | | US-53 Anthropic backend, US-54 per-agent models, US-56 orchestrator workflow | US-55 Anthropic scoreboard | Automatic model routing, other cloud providers |

---

## Must have

### LM Studio and agent core

| ID | Story | Accepted when | Tasks |
|---|---|---|---|
| **US-01** | As a developer, I want to connect to my LM Studio server and see replies stream, so that the agent works with models I already run. | Replies stream as they arrive. A stopped server or missing model gives a message saying what to do. | T1.1, T1.2 |
| **US-02** | As a developer, I want the praxis to load my model with the context size it needs, so that prompts aren't silently cut short. | Context comes from the load settings. A reload needs my consent unless `auto_load` is on. | T1.4 |
| **US-03** | As a developer, I want to give a task in the terminal and have the agent read, edit and run things, so that it can finish real coding work. | One-shot and REPL modes both fix a seeded bug with a real model. | T1.5, T1.6, T1.7, T1.9 |
| **US-04** | As a developer, I want to stop a turn at any moment, so that a bad run can't keep going. | Ctrl-C stops the model and running tools, including child processes. The next prompt continues from consistent history. | T1.7, T1.9 |

### Local model reliability

| ID | Story | Accepted when | Tasks |
|---|---|---|---|
| **US-05** | As a developer, I want malformed tool calls recovered or sent back to the model, so that weaker models can still finish a task. | Every malformed recording is repaired or returned as an error. Invalid calls never execute. | T2.2 |
| **US-06** | As a developer, I want per-model settings detected and overridable, so that each model runs with suitable defaults. | Built-in, LM Studio-reported and user settings layer correctly. | T2.3 |
| **US-07** | As a developer, I want large outputs kept within the context budget, so that one big file or log doesn't crowd out the task. | Oversized results are truncated, and the rest can be read back. Prompt truncation triggers a warning. | T2.4 |
| **US-08** | As a developer, I want later turns to avoid reprocessing the whole prompt, so that I spend less time waiting. | Consecutive requests share a byte-identical prefix, and a real run shows a measured improvement. | T2.5 |
| **US-09** | As a developer, I want long tasks to continue when the context fills up, so that work isn't lost partway through. | Old tool output is masked in rare batches, with a summary only if that isn't enough. The prefix stays intact, and the large-file eval passes on a small context. | T2.6 |
| **US-10** | As a maintainer, I want an eval scoreboard for each model, so that I can prove the praxis improves results. | One command runs all eval tasks and writes results for that model, including turns, wall time, re-fetches and tokens processed versus cached. | T2.1 |

### Safety

| ID | Story | Accepted when | Tasks |
|---|---|---|---|
| **US-11** | As a developer, I want to approve writes and shell commands, so that nothing changes without my say. | Prompts show full parameters. Rejected or unanswered requests change nothing. | T1.8 |
| **US-12** | As a developer, I want permission rules and modes, so that I can allow safe actions and avoid constant prompts. | Every branch of the decision flow is tested. Compound shell commands never match an allow rule. | T3.2 |
| **US-13** | As a developer, I want shell commands sandboxed, so that a bad command can't reach my secrets or the rest of my system. | Sandboxed commands can't write outside the workspace, read `~/.ssh` or open TCP connections. It refuses to run if the sandbox is unavailable. | T3.3 |
| **US-14** | As a developer, I want a cloned repo to be unable to run anything or widen permissions without my approval, so that opening untrusted code is safe. | The hostile fixture repo activates nothing. Changed content needs fresh approval. | T3.4 |
| **US-15** | As a developer, I want the agent kept away from its own config and git hooks, so that it can't approve itself or plant code. | Hard denials hold in every mode for `Write` and `Edit`, and paths can't escape through symlinks. The known `Bash` limit is documented. | T1.5, T3.5 |

### Config and sessions

| ID | Story | Accepted when | Tasks |
|---|---|---|---|
| **US-16** | As a developer, I want layered TOML config, so that I can set defaults and per-project overrides. | Precedence is tested, and errors name the file and key. | T3.1 |
| **US-17** | As a developer, I want to resume a session, so that a restart or crash doesn't lose my work. | Resume restores history, and a truncated last log line is tolerated. | T3.7 |

### MCP

| ID | Story | Accepted when | Tasks |
|---|---|---|---|
| **US-18** | As a developer, I want to use local MCP servers, so that the agent can reach tools beyond the built-ins. | Tools from a stdio server work through the agent loop. A slow server doesn't delay startup. | T4.1 |
| **US-19** | As a developer, I want MCP tools permissioned and failures contained, so that a bad server can't harm me or end my session. | Per-tool rules apply, and a crash returns tool errors and reconnects once. | T4.3 |
| **US-20** | As a developer, I want many MCP tools to fit a small context window, so that local models can still use them. | Large servers switch to indexed mode. Real filesystem, git and browser servers don't lower eval scores. | T4.4, T4.5 |
| **US-44** | As a developer, I want to be told when an approved MCP tool changes its definition, so that a server can't quietly turn malicious. | A changed tool is hidden until I approve it again, and I can see what changed. | T4.6 |

### VS Code

| ID | Story | Accepted when | Tasks |
|---|---|---|---|
| **US-21** | As a VS Code user, I want a chat panel with streaming, tool activity, permission prompts and a stop button, so that I can run the agent from my editor. | The checklist passes, including keyboard-only use. Script and `javascript:` links in model text render inert. A status line shows phase, elapsed time and context used, and tool output starts collapsed. | T5.1, T5.2, T5.3 |
| **US-22** | As a VS Code user, I want proposed edits shown in the native diff view, so that I can review before accepting. | Accept applies the change, reject leaves the file unchanged, and the model is told which. | T5.4 |
| **US-23** | As a VS Code user, I want to install the extension from a local file with no network, so that it stays fully local. | The `.vsix` installs on a clean profile and a full fix-review-accept cycle works offline. | T5.6 |

### Plugins

| ID | Story | Accepted when | Tasks |
|---|---|---|---|
| **US-24** | As a plugin user, I want to install a plugin pinned to a commit and see what it can run first, so that I know what I'm trusting. | Installing from a path or git pins a commit and lists what it can run before enabling. Paths that escape the plugin are rejected. | T6.1, T6.6 |
| **US-25** | As a plugin user, I want plugin skills and commands to load on demand, so that they don't use up the context window. | Only the index sits in the prompt, and the prefix test still passes. | T6.2 |
| **US-26** | As a plugin user, I want plugin hooks to run around tool calls, so that existing automation keeps working. | Exit codes and output fields behave as documented. A hook can't override a deny rule or bypass checks by rewriting input. | T6.3 |
| **US-27** | As a plugin user, I want MCP servers bundled in plugins to work, so that I don't have to set them up separately. | Path variables expand and tool names follow the plugin form. | T6.4 |
| **US-28** | As a plugin user, I want existing Claude Code plugins to work unmodified, so that I can reuse what already exists. | A real plugin with a skill, a hook and an MCP server works, and any gaps are recorded. | T6.7 |

### Performance

| ID | Story | Accepted when | Tasks |
|---|---|---|---|
| **US-29** | As a developer, I want startup and memory use measured against targets, so that "fast" is proven rather than assumed. | Every target in SPEC §2 is measured and reported. | T7.2 |

---

## Should have

Painful to leave out, but each has a workaround.

| ID | Story | Accepted when | Tasks | Workaround until then |
|---|---|---|---|---|
| **US-30** | As a developer, I want the agent to stop when it's stuck repeating itself, so that it doesn't waste time. | Each trigger is tested, and repeated calls with different arguments don't trigger it. | T2.7 | `max_turn_requests` caps the run |
| **US-31** | As a developer, I want a repo map instead of whole-file reads, so that large repos fit in a small context. | Symbols are extracted per language, and eval token use drops without lowering success. | T2.8 | `Grep` and `Read` with line ranges |
| **US-32** | As a developer, I want `AGENTS.md` loaded automatically, so that the agent follows project rules. | It stays within budget, is trimmed with a warning, and can't change permissions. | T3.6 | Point to it in the prompt |
| **US-33** | As a developer, I want to undo the agent's file changes, so that a bad turn is cheap to reverse. | Edited and newly created files are restored. The UI says `Bash` effects can't be undone. | T3.8 | `git` |
| **US-34** | As a developer, I want remote MCP servers and project `.mcp.json` files, so that I can share and reach servers beyond stdio. | Both config shapes load, and header secrets stay out of logs. | T4.2 | Local stdio servers set in config |
| **US-35** | As a VS Code user, I want my active file, selection and unsaved changes available, plus a model picker, so that I don't have to spell out context. | `Read` returns unsaved buffer contents, and I can switch models in the panel. | T5.5 | Name files in the prompt, set the model in config |
| **US-36** | As a plugin user, I want plugin agent definitions to run, so that specialised agents can take delegated work. | A delegated agent runs sequentially with its own history and tool limits. | T6.5 | Use skills instead |
| **US-37** | As a plugin user, I want plugins to ask for their settings and keep secrets safe, so that I don't edit config by hand. | Sensitive values stay out of logs and config. | Part of T6.4 | Set environment variables |
| **US-53** | As a hybrid user, I want to opt in to a Claude model through the same agent loop, so that hard tasks don't depend on what my GPU can run. | The same tools, permissions and sandbox apply. Only user config or a flag enables it, a repository can't, and the first request says content is leaving the machine. | T8.1-T8.5 | Use a separate tool for hard tasks |
| **US-54** | As a hybrid user, I want each delegated agent to use its own model, local or Claude, so that each job runs on a model that fits it. | An agent's `model` resolves through aliases to either backend, falls back to the session model, and never switches to a disabled backend. | T8.6 | Restart the session with another `--model` |
| **US-56** | As a hybrid user, I want a larger model, Claude or a large local one, to plan and send commands to local worker models, so that hard tasks get a strong planner while most work stays local. | The orchestrator is any model reference, delegates through agent definitions on local models, sees only their reports, and the same task runs with a Claude or a local orchestrator by config alone. | T8.8 | Delegate by hand from a separate Claude session |
| **US-57** | As a Claude Code user, I want `CLAUDE.md`, `CLAUDE.local.md`, `~/.claude/CLAUDE.md` and `.claude/rules/` read as Claude Code reads them, with `@imports`, so that my existing memory files work. | Order, fallback and import limits follow §12, imports can't leave the workspace, and the prefix test still passes. | T9.1, T9.2 | One root `AGENTS.md` (US-32) |
| **US-58** | As a developer, I want a subdirectory's memory file and path-scoped rules loaded when the agent first touches a file there, so that local conventions apply without filling the prompt. | Each file arrives once as an appended message, and the prefix is unchanged. | T9.3, T9.6 | Put everything in the root file |
| **US-59** | As a Claude Code user, I want my project and user skills, commands and agents in `.claude/` and `~/.claude/` to work without packaging them as a plugin, so that I can reuse them as they are. | They load with Claude Code's precedence, `/name` runs them, and permission frontmatter and shell injection warn instead of running. | T9.4, T9.5, T9.7 | Wrap them in a local plugin |
| **US-60** | As a Claude Code user, I want my `.claude/settings.json` permission rules and hooks honoured, so that I don't rewrite them in TOML. | User settings apply; project allow rules and hooks wait for trust approval; deny rules always apply. | T9.8 | Copy rules into `config.toml` |
| **US-45** | As a developer, I want repeat reads of unchanged files to be cheap and stale files flagged, so that context isn't wasted and the agent isn't misled. | A stub replaces unchanged content, with full content once masked or changed. Evals show no loss. | T2.10 | Larger context |
| **US-46** | As a developer, I want noise removed from command output, so that logs don't crowd out the task. | Exit codes, failures and stack traces are always kept, and the full output stays available. Evals show no loss. | T2.11 | Truncation from US-07 |
| **US-47** | As a developer, I want malformed tool calls retried under a schema where supported, so that fewer turns are wasted. | The retry is still validated, or an ADR records that LM Studio can't do it. | T2.12 | Plain retry from US-05 |
| **US-48** | As a developer, I want the agent to state assumptions, disagree when warranted and report only what it verified, so that I can judge its work. | The system prompt covers each point within budget, and eval success doesn't fall. | T2.13 | Ask for this in the prompt |
| **US-49** | As a VS Code user, I want a turn summary built from the tool log, so that I can see what really happened rather than what the model claims. | It lists files, commands and exit codes, and says plainly when no tests ran. | T5.7 | Expand tool items |
| **US-50** | As a VS Code user, I want to review a multi-file change from overview to file to chunk, so that my attention goes where the risk is. | I can move freely between levels and revert per file. Risk labels never read as a safety verdict. | T5.8 | Per-edit diffs from US-22, plus `git` |
| **US-51** | As a VS Code user, I want a plan first and to decide when it runs, so that I can correct direction early. | `/plan` can't write or run anything, and carrying out the plan is my separate action. | T5.9 | Use read-only mode and ask for a plan |
| **US-61** | As a developer, I want `/plan`, `/undo` and `/rewind` to work in the terminal REPL, so that I can plan, check and reverse work without VS Code. | `/plan <task>` runs read-only, so writes are refused, and only `/execute` restores my configured mode; `/plan` with no task prints usage. `/undo` restores edited files, says when there is nothing to undo, and always says `Bash` effects aren't undone. `/rewind` with a bad or out-of-range number prints usage or a clear error and changes nothing. | T12.2 | Use the VS Code panel |
| **US-62** | As a developer, I want to list and delete saved sessions from the terminal, so that old sessions don't pile up. | `praxis sessions` lists only this workspace's sessions as text, with titles sanitised. `--delete <id>` removes that session and no other, and `--folder` with the wrong arguments exits with the usage code. Without `HOME` or `XDG_DATA_HOME`, `praxis sessions` says so and exits with the usage code, and a normal run says the session isn't saved and still works. | T12.3 | Delete files in the data directory |
| **US-63** | As a developer, I want to be told why a turn stopped, so that I and my scripts can react. | Hitting the output limit and hitting `agent.max_turn_requests` each print their own message and exit with the failure code; a cancelled turn exits with the cancel code. | T12.4 | Read the log |
| **US-64** | As a developer, I want every MCP failure reported clearly, so that a misbehaving server can't confuse the model or slip past trust checks. | A call to an unknown tool, a call that times out (60 s) and a server returning image, audio or resource content each give a clear tool result instead of an error or raw data. An unreadable trust store hides the server's tools with a notice naming the file. | T12.5 | Watch the server's own logs |

---

## Could have

The first things to cut.

| ID | Story | Tasks |
|---|---|---|
| **US-38** | As a developer, I want a rich terminal UI, so that long sessions are easier to follow. | Cut (2026-11-12): the REPL stays |
| **US-39** | As a developer, I want MCP resources and prompts, so that servers can offer more than tools. | Part of T4.5 |
| **US-40** | As a developer, I want to use the praxis from Zed or another ACP editor, so that I'm not tied to VS Code. | Part of T5.6 |
| **US-41** | As a VS Code user, I want editor diagnostics available as context, so that the agent can see current errors. | Part of T5.5 |
| **US-42** | As a plugin user, I want plugin `bin/` tools on the agent's `PATH`, so that plugins can ship command-line tools. | Part of T6.4 |
| **US-43** | As a VS Code user, I want the extension to work in VSCodium, so that I can avoid the proprietary build. | Part of T5.6 |
| **US-52** | As a maintainer, I want a timed trial of real tasks with and without the praxis, so that its value is measured rather than felt. | T7.5 |
| **US-55** | As a maintainer, I want a scoreboard on a Claude model, so that local results have a reference ceiling. | T8.7 |

---

## Won't have (this release)

Not stories to build, but recorded so nobody builds them by accident.

| Feature | Why not | Revisit if |
|---|---|---|
| **Ollama, other OpenAI-compatible servers, cloud providers other than Anthropic** | LM Studio plus opt-in Anthropic covers the need ([ADR 0004](adr/0004-anthropic-backend-and-per-agent-models.md)) | Those two stop meeting the need |
| **Hosting or running models** | The praxis connects to a server | Not planned |
| **Parallel subagents** | One GPU means contention or model swaps | Hardware allows real parallelism |
| **Automatic model routing** | Swapping local models costs load time; US-54 lets the user choose per agent instead | M2 measurements show a gain |
| **Embeddings or RAG index** | The repo map and `Grep` come first | Evals show they aren't enough |
| **Plugin marketplace** | Git URLs are enough | Plugin sharing becomes common |
| **VS Code Marketplace publishing** | Local `.vsix` install is enough, and publishing without `vsce` is unresolved | Others want to install it |
| **JS plugin workflows and mods** | They need a JS runtime, which [ADR 0001](adr/0001-rust-only-with-minimal-vscode-glue.md) excludes | The Rust-only rule changes |
| **Plugin LSP, monitors, output styles, themes, channels, `.mcpb` bundles** | Not needed for the core plugin experience, so they warn on load | Real plugins depend on them |
| **MCP OAuth** | Static headers cover local use | A needed server requires it |
| **Windows and macOS sandboxing** | Linux first | Those platforms are targeted |
| **Telemetry** | Conflicts with the project's purpose | Never |
| **Prompt compression (LLMLingua) and minification** | They lower success on code, and minifying would break `Edit`'s exact matching ([evidence](RESEARCH.md#token-reduction)) | New evidence on code |
| **Helper models** for compression or tool calls | One GPU can't spare the memory or time | Hardware allows it |
| **Proactive suggestions** | They keep inference running on the only GPU, and the evidence comes from short student tasks | Developers ask for it after real use |
| **Automatic LLM review verdicts** | Confident verdicts risk false assurance, and a plugin skill can do this on request | Evidence improves |

---

## Enabling work

These tasks have no user story because they make the rest possible. They aren't counted in the scope check.

| Tasks | Purpose |
|---|---|
| T0.0, T0.1 | Prerequisites and repository setup |
| T0.2, T0.3, T0.4, T0.5, T0.6 | Baseline, spikes and eval tasks |
| T2.9 | M2 go/no-go checkpoint |
| T7.1, T7.4 | Documentation, threat model and release |

---

## Is this scope realistic?

Task counts are only a rough proxy for effort.

| Scope | Must tasks | Share of 60 feature tasks |
|---|---|---|
| **As requested** | 37 | 62% |
| Plugins moved to Should | 31 | 52% |
| Plugins and VS Code moved to Should | 26 | 43% |

The research work added one Must task and eight optional ones, so the share fell from 82% only because the total grew. Must itself got slightly bigger. Moving plugins out of Must now reaches the 60% guideline, but that's a decision for the project owner. M8 (ADR 0004) added seven Should or Could tasks and no Must ones, so as requested the share is now close to the guideline. Plugins are the natural candidate: they come last, they build on MCP, and a working release is still useful without them.

> [!NOTE]
> Should and Could tasks are marked in [TASKS.md](TASKS.md). No Must task depends on one, so they can be cut without blocking anything. Agents still build them in order unless the project owner says otherwise.
