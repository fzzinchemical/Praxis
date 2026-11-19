## 12. Plugins

Reference: [manifest](https://code.claude.com/docs/en/plugins/manifest-reference.md), [components](https://code.claude.com/docs/en/plugins/components.md), [hooks](https://code.claude.com/docs/en/hooks.md). The formats belong to Claude Code and can change. Implement the subset below and warn clearly about anything skipped; never fail silently.

### Support matrix

| Component | Location | First version |
|---|---|---|
| Manifest | `.claude-plugin/plugin.json`, optional; only `name` is required | Yes. Unknown top-level keys are warnings |
| Skills | `skills/<name>/SKILL.md` | Yes |
| Commands | `commands/*.md` (same frontmatter as skills) | Yes, invoked as `/<plugin>:<name>` |
| Agents | `agents/**/*.md` | Sequential only; `name`, `description`, `model`, `tools`, `disallowedTools`, `maxTurns`. `model` values like `sonnet` map through `[models]` aliases (§8), possibly to another backend, or fall back to the session model. The session model can act as an orchestrator that delegates to local workers ([ADR 0005](../adr/0005-orchestrator-model-delegates-to-local-workers.md)). An agent sees only the tools `tools` and `disallowedTools` let it use, including MCP tools in the indexed-mode index and `ToolSearch` results |
| Hooks | `hooks/hooks.json` | `type: "command"` only |
| MCP servers | `.mcp.json`, with or without wrapper; `mcpServers` in manifest | Yes; `.mcpb` bundles no |
| `userConfig` | Manifest | Yes; `sensitive` values kept out of config and session logs, and stored in a `0600` file |
| `bin/` | Plugin root | Appended after the user's `PATH`, so it can't shadow system commands |
| LSP, monitors, output styles, themes, channels, marketplace | | No; warn |
| Workflows and mods | JavaScript files | **No; they need a JS runtime, which ADR 0001 excludes** |

Path rules from the manifest reference: component paths must begin with `./` (`skills` may also be `.`), must not contain `..`, and must resolve inside the plugin root. A path that doesn't exist is skipped with a warning, so one stale manifest path doesn't block install. `commands` and `agents` replace the default scan, `skills` adds to it, and `hooks` and `mcpServers` merge with their default files.

A built-in `general` agent is always offered through the same `Agent` tool, so delegation works with no plugin installed: it gets every session tool except `Agent` and `Skill` (as every subagent does), has no `model` (so it runs on the session model), and its system prompt asks for a short report of what it found or changed. A plugin agent named `general` is an error at load, naming the plugin.

Plugins are never read from `~/.claude`. Install them explicitly into `~/.local/share/praxis/plugins/`. Names read from `installed.json` are validated on every read with the install rule, so a stored name like `../x` can't reach outside the store. `praxis plugin install <path|git URL>` clones into `plugins/<name>/` (a local path may be a plugin subdirectory of a marketplace repo: the repo is cloned and only that subdirectory kept), pins the HEAD commit in `plugins/installed.json`, prints what it can run (hooks, MCP servers, `bin/`) and leaves it disabled; `enable` prints that list again. `remove` keeps `plugins/data/<name>/` unless `--delete-data` is passed or a terminal confirms. Each session (CLI and `praxis acp`) loads enabled plugins at start: skills join the index, agents are offered through an `Agent` tool (`subagent_type`, `prompt`) that runs one in a fresh history with only its allowed tools and returns its last reply, `PreToolUse` hooks run in the permission check, and MCP servers connect and are pinned. `/<plugin>:<cmd>` works in the CLI only; other hook events don't run yet. A subagent's text and tool calls go to the front end tagged with its `Agent` call (SPEC §11, `_meta.praxis`); the VS Code panel nests them under that call, collapsed, with a summary such as "Explore · 6 tool calls · running" (T10.8). The panel's Stop button on a running subagent sends `_praxis/cancel_subagent` for it (T10.12).

`plugin list --json` also carries each plugin's `readme` (its root `README.md` text, or `null`, never read through a link out of the plugin root) and `components`, one `{kind: command|skill|agent, name, description}` per command, skill and agent. The VS Code Plugins view opens a read-only details tab per plugin on click (T10.9): the README rendered with raw HTML escaped and unsafe links dropped, then the components. The page is built from the listing already in memory, under a CSP with no scripts. Agents are listed without resolving `model`, so one whose model is not enabled still shows.

Each listed plugin also carries `settings`, one `{key, description, sensitive, set, value}` per `userConfig` key; `value` is null for a `sensitive` key, which shows only `set`. `praxis plugin config <plugin> <key> <value>` stores one value (`set_option`). The details tab's Settings section links each key to a `command:praxis.setPluginOption` URI, which asks for the value in a masked input box for a `sensitive` key and then runs that command.

### Skills

The prompt has an index of `name: description`, frozen at session start and subject to the budget. The `Skill` tool returns the body on demand, as an appended message. Substitute `$ARGUMENTS` (`UNVERIFIED` for the full list of substitutions; see Claude Code's skills doc).

### Markdown sources outside plugins

Decided in [ADR 0007](../adr/0007-markdown-sources-like-claude-code.md). Project markdown is untrusted context: it never grants permissions, starts processes or enables a backend.

**Memory files**, in prefix order (broadest first, nearest last):

1. User: `~/.claude/CLAUDE.md`, then `~/.config/praxis/AGENTS.md` (or under `$XDG_CONFIG_HOME`).
2. Each directory from the workspace root down to cwd: `CLAUDE.md` or `.claude/CLAUDE.md`, then `CLAUDE.local.md`. `AGENTS.md` is read only if no `CLAUDE.md` exists in the workspace (Claude Code's default). Never above the workspace root.
3. Rules without a `paths` field: `~/.claude/rules/**/*.md`, `~/.config/praxis/rules/**/*.md`, then `.claude/rules/**/*.md` and `.praxis/rules/**/*.md`.

User files (1) are the user's own, so each is prefixed with `Instructions from: <path>` and a blank line. Project memory files (2) are repository content: they are labelled the same way only once trusted, by the approval in §8 (T13.6). An unapproved file is loaded as plain context, which is safe because it is only text the model reads. Rule files (3) and nested memory found mid-session are never labelled; the nested case is a shortcut until the agent can reach the trust store. The system prompt tells the model to follow the rules under each `Instructions from:` heading.

All of them sit in the frozen prefix under the existing budget, trimmed broadest-first with a warning. Block-level HTML comments outside code are stripped.

**Imports:** `@path` on a memory file's line imports that file in place. Relative paths resolve against the importing file; `@~/` against home. Up to four hops; a cycle stops silently at the repeated file. All imports from one memory file share a 256 KiB budget; past it, no further imports expand. Not parsed inside code spans or fenced blocks. A project file's import must resolve inside the workspace (confinement rules, §9) and a user file's inside `~/.config/praxis/`; otherwise it's skipped with a warning.

**Index lines (lazy memory):** a line in the user's own `~/.claude/CLAUDE.md` or `~/.config/praxis/AGENTS.md` of the form `<path> — read when <trigger>` (or ` - read when`) is left as text, and `Read` may open the named file outside the workspace, so a small model sees only the core and reads a file when its trigger applies. `~/` resolves against home, a relative path against the memory file's folder. Project files' index lines grant nothing (`context::prefix::index_paths`).

**Nested memory** (*Should*): the first time `Read`, `Write` or `Edit` touches a file under a subdirectory of cwd, that directory's memory file, and any rule whose `paths` glob matches the file, is appended as a message. Each is loaded once per session.

**Skills, commands and agents:**

| Kind | User | Project | Name |
|---|---|---|---|
| Skills | `~/.claude/skills/<name>/SKILL.md`, `~/.config/praxis/skills/` | `.claude/skills/`, `.praxis/skills/` | `name` field, else directory |
| Commands | `~/.claude/commands/**/*.md`, `~/.config/praxis/commands/` | `.claude/commands/`, `.praxis/commands/` | File stem; subdirectories join with `:` (`frontend/component.md` is `frontend:component`) |
| Agents | `~/.claude/agents/**/*.md`, `~/.config/praxis/agents/` | `.claude/agents/`, `.praxis/agents/` | `name` field |

- Skills and commands share one namespace, invoked as `/<name>` in the CLI. A skill beats a command of the same name. On a clash, user beats project for skills and commands, and project beats user for agents (Claude Code's order). `.praxis/` beats `.claude/` at the same level, so a praxis-only override is possible. Plugin entries stay namespaced and never clash. A standalone agent named `general` is an error, as for plugins.
- Frontmatter read: `name`, `description`, `when_to_use` (appended to the description), `argument-hint`, `disable-model-invocation` (left out of the index), `user-invocable: false` (no `/` command), `model`; agents also take `tools`, `disallowedTools`, `maxTurns` as in §12. `allowed-tools`, `permissionMode`, `hooks`, `mcpServers` and `` !`cmd` `` injection warn and are ignored. Other unknown fields are ignored silently, as in Claude Code. An agent whose `maxTurns` is zero or not a number is skipped with a warning.
- Substitutions in skill and command bodies: `$ARGUMENTS`, `$ARGUMENTS[N]` and `$N` (0-based, split like a shell), `${CLAUDE_SKILL_DIR}`, `${CLAUDE_PROJECT_DIR}`, `${CLAUDE_SESSION_ID}`; `\$` escapes. With no placeholder, `ARGUMENTS: <value>` is appended.
- **Settings:** `~/.claude/settings.json` sits just below the user layer and `.claude/settings.json` plus `.claude/settings.local.json` just below the project layer (§8). Only `permissions.allow`, `permissions.deny` and `hooks` are read, in Claude Code's rule syntax, which §9 already uses; other keys are ignored without a warning, because they're Claude Code's own. Project allow rules and hooks are widening items for the trust gate. A malformed settings file is a config error naming the file, never skipped, because it carries permission rules.
- Unreadable or malformed files are skipped with a warning naming the path; never fail silently.

### Hooks

```json
{ "hooks": { "PostToolUse": [ { "matcher": "Write|Edit",
  "hooks": [ { "type": "command", "command": "\"${CLAUDE_PLUGIN_ROOT}/scripts/format.sh\"", "timeout": 30 } ] } ] } }
```

Files wrap the event map in `"hooks"`; an inline manifest object doesn't.

**Events supported:** `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `SessionStart`, `Stop`. Warn on any others.

**Matcher:** `*`, an empty string or an omitted value matches all. Only letters, digits, `_`, `-`, spaces, `,` and `|` means an exact name or list. Anything else is an unanchored regex; Claude Code uses JS regex, and Rust's `regex` has no lookaround, so warn and skip on a pattern that doesn't compile.

**Execution:** with `args`, run with no shell (exec form). Without `args`, run through the shell, because `command` may be a shell string. Default timeout 30 s; on timeout, kill its whole process group and treat it as a non-blocking error. `args` that aren't a list of strings reject the hook. Run hooks only from approved plugins.

**Stdin JSON:**

| Event | Fields, in addition to `session_id`, `transcript_path`, `cwd`, `permission_mode`, `hook_event_name` |
|---|---|
| `PreToolUse` | `tool_name`, `tool_input`, `tool_use_id` |
| `PostToolUse` | Those, plus `tool_response`, `duration_ms`, and `mcp_server` for MCP tools |
| `UserPromptSubmit` | `prompt` |
| `SessionStart` | `source` (`startup`, `resume`, ...), `model` |
| `Stop` | `UNVERIFIED`; read the hooks doc section before implementing |

**Exit codes:** 0 means parse stdout as JSON if it's a single JSON object, otherwise plain text (added to context only for `UserPromptSubmit` and `SessionStart`). 2 blocks where the event can block, with stderr as the reason, even if JSON allowed it. Anything else is a non-blocking error and the action proceeds.

**Output handled:** `continue`, `stopReason`, `systemMessage`, top-level `decision: "block"` with `reason`, and `hookSpecificOutput` with `hookEventName`, `permissionDecision` (`allow`, `deny`, `ask`, `defer`; treat `defer` as no decision), `permissionDecisionReason`, `updatedInput`, `additionalContext`, `updatedToolOutput`.

Revalidate `updatedInput` against the schema, then run the permission check on the new input. Otherwise a hook-altered path could bypass the check.

`additionalContext` is appended as a message, keeping the prefix stable.

**Environment for hooks and plugin MCP servers:** `CLAUDE_PLUGIN_ROOT`, `CLAUDE_PLUGIN_DATA` (mapped to `~/.local/share/praxis/plugins/data/<id>/`), `CLAUDE_PROJECT_DIR`, `CLAUDE_PLUGIN_OPTION_<KEY>`. `${...}` expands in `command`, `args`, `env` and skill or agent bodies. These names exist for compatibility with existing plugins.

### MCP tool names

`mcp__<server>__<tool>`; for plugins, `mcp__plugin_<plugin>_<server>__<tool>`. Use these in permission rules and matchers. A server or plugin name containing `__` is rejected when config or plugins load, since it could make two tool names collide; a colliding tool is skipped.

