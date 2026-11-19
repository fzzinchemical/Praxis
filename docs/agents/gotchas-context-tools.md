# Gotchas: Context, prompt cache, tools, MCP

- Adding a tool schema mid-session changes the prompt prefix and throws away the server's prompt cache (SPEC §7).
- Saving tokens can make things slower: any rewrite of old history forces reprocessing from that point. Judge context features by turns and wall time, not just tokens (SPEC §7, §14).
- Don't add a helper model. Papers that report large savings often use one, and there's only one GPU (docs/RESEARCH.md).
- Built-in tool and parameter names are a compatibility surface for plugin hooks. Don't rename `Bash`, `Edit`, `file_path` and so on (SPEC §6).
- Grep defaults to `content`, unlike Claude Code. With file names only, small models re-grep in loops (SPEC §6).
- The real MCP servers (`docs/MCP_SERVERS.md`) need `PLAYWRIGHT_BROWSERS_PATH` set when `HOME` is overridden, and `playwright-mcp` needs `--browser chromium`. Its `file:` URLs are blocked, so use `data:` URLs.
- Mid-turn memory and scoped rules are recorded as `Message::User` with `MEMORY_MARK`, so count turns with `Message::starts_turn()`, never by counting user messages (rewind, resume, `cut_turn`).
- MCP server `instructions` are appended to each of that server's tool descriptions, after the tool's own text, because the tool-search index shows only the first line of a description.
