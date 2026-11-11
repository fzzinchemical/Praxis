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
- T6.7 "Done when" is not met. This plugin has no MCP server and no `PreToolUse` hook, so a second plugin is needed to prove those parts.
