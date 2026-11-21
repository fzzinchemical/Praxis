## 10. Security invariants

> [!CAUTION]
> Every one needs a test. Never weaken these without human approval.

1. Nothing the model outputs or reads can change permissions, config or trust.
2. A repository can't run code or widen permissions without approval, and changed content requires re-approval. Exception: MCP servers, project ones included, are trusted and start without approval (owner decision 2026-11-21).
3. `Bash` never runs outside the sandbox unless the user turned it off in **user** config or a flag.
4. Write and edit paths are checked after symlink resolution.
5. **Hard denials,** even under `auto-edit` and `full-access` (§9, T10.7): `.git` (the file or folder, at any depth, so submodule `.git/modules/*/hooks` and worktree `.git` files count), `.praxis/**`, `.mcp.json`, and user config. Otherwise the agent could approve itself or plant code that runs later. `Bash` has a documented limit (§9): the text check is best effort. A command that builds a `.git` path at run time (an interpreter or a variable) can still write `.git` objects, refs and index, and only the prompt stops that. `.git/hooks` and `.git/config` are read-only under the Git guard (§9) when `bwrap` is present, so a planted hook or alias can't run later; without `bwrap`, the prompt is the only stop for them too.
6. Secrets don't appear in logs, session records or child environments. Keys are referenced by variable name.
7. Tool output, MCP results and file contents are treated as data: never parsed for tool calls, and shown escaped in UIs.
8. No network traffic except to the configured LM Studio server, the Anthropic API when enabled in user config or a flag, MCP servers, and whatever the user approves.
9. Plugins are pinned to a commit, and what they can run is shown before enabling.
10. In the webview, model text never becomes raw HTML (§11).
11. An MCP tool whose name, description or schema changed since it was last seen is reported with a notice and its scan warnings; it stays offered, since MCP servers are trusted (§13, owner decision 2026-11-21).
12. Project config, plugins and agent definitions can't enable a cloud backend or change its URL or key variable. An agent's `model` that resolves to a disabled backend is an error, never a silent switch (ADR 0004).

