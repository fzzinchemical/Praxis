# 0007: Markdown memory, skills, commands and agents outside plugins

Date: 2026-11-12. Status: accepted.

## Context

The owner wants the praxis to work like Claude Code, where behaviour is shaped by plain markdown files. Today the praxis reads one `AGENTS.md` at the workspace root, and skills, commands and agents only from enabled plugins (§12). Claude Code also reads ([memory](https://code.claude.com/docs/en/memory.md), [skills](https://code.claude.com/docs/en/skills.md), [subagents](https://code.claude.com/docs/en/sub-agents.md), checked 2026-11-12):

- memory files from the user directory, from every directory between the filesystem root and cwd, and lazily from subdirectories once a file in them is touched;
- `@path` imports in memory files, up to four hops deep, skipped inside code spans and blocks;
- `.claude/rules/*.md`, optionally scoped by a `paths` glob;
- standalone skills (`skills/<name>/SKILL.md`), commands (`commands/**/*.md`, merged into skills) and agents (`agents/**/*.md`) from user and project directories.

## Decision

Support these, with these differences, so the existing safety model holds:

- **Claude Code compatibility comes first** (owner, 2026-11-12). Where the praxis can follow Claude Code's behaviour safely, it does, and `.claude/` is the main location.
- **Claude Code's file order.** In each directory, `CLAUDE.md` (or `.claude/CLAUDE.md`), then `CLAUDE.local.md`; `AGENTS.md` only when no `CLAUDE.md` exists, as in Claude Code's default.
- **Stop at the workspace root.** Ancestors above the workspace aren't read, so nothing outside confinement (§9) reaches the prompt. User files come from `~/.claude/` first (`CLAUDE.md`, `rules/`, `skills/`, `commands/`, `agents/`), then `~/.config/praxis/`. No managed-policy files.
- **Imports stay confined.** A project file's `@path` must resolve inside the workspace; a user file's inside `~/.config/praxis/`. Anything else is skipped with a warning, not offered for approval.
- **Directories.** Project: `.claude/`, plus `.praxis/` for praxis-only files. Plugins are still never read from `~/.claude` (§12): they hold executable code and need the install-and-approve flow.
- **Settings.** `.claude/settings.json` and `~/.claude/settings.json` `permissions` and `hooks` are read as config layers below the praxis's own. Project ones pass the trust gate (§8) like `.praxis/config.toml`.
- **Markdown can't grant anything.** Like `AGENTS.md` today, project markdown is context. `allowed-tools`, `permissionMode`, `hooks` and `mcpServers` in frontmatter are ignored with a warning. Skill shell injection (`` !`cmd` ``) isn't supported: it would run commands outside the permission check.
- **Reuse the plugin parser** (`plugin.rs` `read_markdown`) rather than writing a second one.

## Consequences

- A repository or user setup written for Claude Code gets its rules, skills and agents with no changes, except shell injection and permission frontmatter, which warn.
- Lazily loaded memory and rules arrive as appended messages, keeping the prefix byte-identical (§7).
- More files can enter the prefix, so the existing budget (25% of `C`) and its trim warning now cover all of them, nearest files kept first.
