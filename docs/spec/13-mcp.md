## 13. MCP

`rmcp` targets spec `2026-07-28`. Check the client API on docs.rs for the pinned version; its README doesn't include a client example.

- **Transports:** stdio child process and streamable HTTP. No OAuth at first: static headers via `headers` in config, with values from environment variables.
- **Lifecycle:** connect in the background at session start, so a slow server doesn't delay the prompt. Listings after connect run concurrently under the connect timeout. A failed server produces a notice and the session continues without it. If it crashes mid-session, its tool calls return errors, and reconnect once.
- **Tool list freeze:** `tools/list` results are fixed at session start per §7, with later `list_changed` notifications logged rather than applied.
- **Results:** pass through the tool-result budget; non-text content is replaced by a short description.
- **Resources and prompts:** a server that declares them gets one `mcp__<server>__read_resource` tool (enum of its URIs) and one `mcp__<server>__get_prompt` tool (enum of its prompt names). They go through the same permission, hook, pin and budget paths. A server tool with the same name wins.
- **LM Studio servers:** `~/.lmstudio/mcp.json` loads as an extra source (§8). It loads without approval, like every MCP server, and is pinned like any other server.
- **Trust:** MCP servers are trusted from every source, project ones included, and never ask for approval (owner decision 2026-11-21).
- **Pinning:** at connect, store the SHA-256 of each tool's name, description and schema. Pins live in the trust store under `mcp:<tool name>`, beside the definition text. A changed tool is reported with a notice and its scan warnings, re-pinned, and stays offered. An unreadable store is a notice; the tools stay offered. This also covers plugin servers.
- **Description scan:** for a new or changed tool, flag instruction-like wording, encoded content and unusual length as a warning only. It catches part of the problem and has false positives, so it never blocks and never counts as a defence.
- **Timeouts:** default 60 s per call, and cancellation is forwarded.
- **Server list:** the prompt's prefix lists each connected server that is not off: its name, its tool names, and its `initialize` `instructions` capped at 500 characters, collapsed to one line. MCP servers are trusted, so the notes carry no untrusted label. Sorted by name, so the prefix stays stable. Tool names already carry the server (`mcp__<server>__<tool>`); the notes appear only here, not in each tool description.
- **Schemas:** weak models choke on large schemas, so strip `$comment`, `examples` and long descriptions in the prompt, but validate against the original.

- **Session toggles (T4.7):** a session can turn a server off. Its tools leave `tools` (or the indexed list and `ToolSearch`), and calls to them fail with a tool error saying the server is off. The change applies from the next request, since a changed tool list breaks the prompt cache (§7). The set is kept per session and never saved.

Start with tools, then resources and prompts.

