# Plugin compatibility

Results of running unmodified Claude Code plugins through `harness plugin install` (T6.7, SPEC §12).

## DietrichGebert/ponytail

- Source: https://github.com/DietrichGebert/ponytail, commit `96608e078b2d5ea607c5c3b8b5a54d1c1fba6846` (version 5.3.3), checked 2026-11-12.
- How: cloned into a temp dir, installed with `harness plugin install <path>`, then enabled, with `XDG_DATA_HOME` and `XDG_CONFIG_HOME` pointed at temp dirs. A one-shot session ran against a local fake OpenAI-compatible server (not LM Studio). The server scripted a `Skill` call and logged each request.
- Contents: 6 skills, 6 Gemini-style `commands/*.toml`, hooks for `SessionStart`, `SubagentStart` and `UserPromptSubmit`, a JS "band" mod, and `userConfig`. It has no agents and no MCP server.

| Part | Result |
|---|---|
| Install, pin, enable | Works. `installed.json` pins the commit, and install and enable list the runnable hooks. |
| Manifest `hooks` given as an array of file paths | Works. Both files load. |
| Skills index | Works after a fix. All 6 `ponytail:*` skills appear in the prompt index. Before the fix, each description read `>` because the frontmatter parser didn't understand YAML block scalars (`description: >`). Block scalars are now folded to one line. |
| `Skill` tool | Works. `Skill {"skill":"ponytail:ponytail"}` returns the body as the tool result. |
| Commands | None loaded. `commands/*.toml` is Gemini CLI's format, and Claude Code also reads only `commands/*.md`. `/ponytail:ponytail-help` is therefore sent to the model as plain text. This matches Claude Code. |
| Hooks | Parsed, but none ran. `SessionStart` and `UserPromptSubmit` are parsed and listed, but only `PreToolUse` runs today (SPEC §12). `SubagentStart` is unsupported and warns. Every hook command is `node -e ...`, so even once those events run, the hooks need Node on the user's machine. That is the plugin's runtime, not the harness's (ADR 0001 governs only product code). The scripts were read but not executed. |
| `claude-band.json` `modules` (JS mod) | Unsupported (ADR 0001). This used to be dropped silently. The harness now warns: ``unsupported key `modules` ignored``. |
| Manifest `types` | Warns as an unknown key, as specified. |
| `userConfig` | Accepted. Its values are only consumed by the band and ledger hooks, which don't run. |
| MCP servers | Not covered. The plugin has none. |
| `PreToolUse` hook | Not covered. The plugin has none. |

### Gaps, not fixed here

- `SessionStart` and `UserPromptSubmit` hooks don't run. Ponytail's main mechanism injects its rules through `SessionStart` `additionalContext`, so without it the plugin only works when the model chooses to load the skill.
- `SubagentStart` is not in the supported event list.
- On its own, this plugin can't meet the T6.7 "Done when": it has no MCP server and no `PreToolUse` hook. See below.

## anthropics/claude-plugins-official: hookify and playwright

No single unmodified plugin found has a skill, a `PreToolUse` hook and a working MCP server. By the owner's decision, T6.7 "Done when" is met with two unmodified plugins from one repo: `plugins/hookify` (skill and `PreToolUse` hook) and `external_plugins/playwright` (stdio MCP server).

- Source: https://github.com/anthropics/claude-plugins-official, commit `e37d307bd9f85dab6fea5dfb5e3dc146753b2d2d`, checked 2026-11-12. hookify is version 0.1.1; `npx` resolved `@playwright/mcp@latest` to 0.0.85.
- How: shallow clone in a temp dir; `harness plugin install <repo>/plugins/hookify` and `<repo>/external_plugins/playwright`, then `enable`, with `XDG_DATA_HOME` and `XDG_CONFIG_HOME` in temp dirs. hookify's Python scripts were read before running. The temp workspace held hookify's own example rule `examples/dangerous-rm.local.md`, copied to `.claude/hookify.dangerous-rm.local.md` as its docs say. User config allowed `Bash(rm *)` and `mcp__plugin_playwright_playwright`, so any block had to come from the hook. A one-shot session ran against a local fake OpenAI-compatible server (not LM Studio) that scripted three tool calls. Playwright's npm cache and browser (Chromium headless shell) went into temp dirs.

| Part | Result |
|---|---|
| Install from a marketplace subdirectory | Works after a fix. Install used to `git clone` the given path, which fails for a subdirectory of a repo. A local path inside a repo now clones the repo, pins its HEAD and keeps only that subdirectory. Git URLs must still point at a plugin repo's root. |
| hookify skill | Works. `hookify:writing-hookify-rules` is in the index (named from its frontmatter), and `Skill` returns the body. |
| hookify `PreToolUse` hook | Works after a fix. `Bash {"command":"rm -rf victim.txt"}` was denied and the file survived. hookify denies with `permissionDecision: "deny"` and puts its message only in `systemMessage`, so the model used to get an empty error. A deny without `permissionDecisionReason` now falls back to `systemMessage`, and the model sees the rule's text. |
| hookify `PostToolUse`, `Stop`, `UserPromptSubmit` | Parsed and listed, not run (only `PreToolUse` runs today, SPEC §12). |
| hookify commands and agent | Not exercised. |
| `hooks/hooks.json` `description` key | Ignored with a warning, as specified. |
| playwright MCP server | Works. It connects as `plugin_playwright_playwright`; its tools appear in the MCP index. `mcp__plugin_playwright_playwright__browser_navigate` to a `file://` page in the workspace returned the page title. |

### Gaps, not fixed here

- Playwright's defaults don't fit this machine without plugin changes, so the run set environment variables instead: `PLAYWRIGHT_MCP_BROWSER=chromium` (no Chrome installed; the default is the `chrome` channel), `PLAYWRIGHT_MCP_HEADLESS=true`, `PLAYWRIGHT_MCP_ISOLATED=true`, and `PLAYWRIGHT_MCP_ALLOW_UNRESTRICTED_FILE_ACCESS=true` (`file:` URLs are blocked by default). The browser was installed with `playwright install chromium` into a temp `PLAYWRIGHT_BROWSERS_PATH`. The harness passes its environment to the server, so these work. They are the plugin's own runtime needs (Node, `npx`, a browser), not harness gaps.
- hookify's `systemMessage` is not shown to the user; the harness ignores `systemMessage` everywhere except as the fallback deny reason above.
- Installing a subdirectory from a git URL is unsupported; clone the marketplace repo locally first.
