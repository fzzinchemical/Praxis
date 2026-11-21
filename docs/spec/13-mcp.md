## 13. MCP

`rmcp` targets spec `2026-07-28`. Check the client API on docs.rs for the pinned version; its README doesn't include a client example.

- **Transports:** stdio child process and streamable HTTP. No OAuth at first: static headers via `headers` in config, with values from environment variables.
- **Lifecycle:** connect in the background at session start, so a slow server doesn't delay the prompt. Listings after connect run concurrently under the connect timeout. A failed server produces a notice and the session continues without it. If it crashes mid-session, its tool calls return errors, and reconnect once.
- **Tool list freeze:** `tools/list` results are fixed at session start per §7, with later `list_changed` notifications logged rather than applied.
- **Results:** pass through the tool-result budget; non-text content is replaced by a short description.
- **Resources and prompts:** a server that declares them gets one `mcp__<server>__read_resource` tool (enum of its URIs) and one `mcp__<server>__get_prompt` tool (enum of its prompt names). They go through the same permission, hook, pin and budget paths. A server tool with the same name wins.
- **LM Studio servers:** `~/.lmstudio/mcp.json` loads as an extra source (§8). It loads without approval and is pinned like any other server.
- **Pinning:** on approval, store the SHA-256 of each tool's name, description and schema. Pins live in the trust store under `mcp:<tool name>`, beside the definition text so a change can be shown; a tool seen for the first time is pinned without asking, since its server was already approved. On any change, hide that tool from the model and ask again, showing what changed. This also covers plugin servers.
- **Description scan:** at approval, flag instruction-like wording, encoded content and unusual length as a warning only. It catches part of the problem and has false positives, so it never blocks and never counts as a defence.
- **Timeouts:** default 60 s per call, and cancellation is forwarded.
- **Server list:** the prompt's prefix lists each connected server that is not off: its name, its tool names, and its `initialize` `instructions` capped at 500 characters, collapsed to one line and labelled untrusted. Sorted by name, so the prefix stays stable. Tool names already carry the server (`mcp__<server>__<tool>`); tool descriptions are not changed, since a change would invalidate stored pins.
- **Schemas:** weak models choke on large schemas, so strip `$comment`, `examples` and long descriptions in the prompt, but validate against the original.

- **Approving project servers:** a server waiting for approval (§8) is listed with status `needs_approval` and the command line its approval would run. In ACP, the MCP view's Approve action shows that command in a modal, and only an explicit confirm sends `_praxis/approve_mcp_server` (§12). The agent stores the approval under the server's own key in the trust store (§8) before it tries the connection, then connects the server for the session and answers `{name, notices}`; a connect failure is a notice, and the approval stays stored. The request is refused at once while a turn runs, and the server's tools apply from the next request. A server whose TOML changes waits for approval again.
- **Session toggles (T4.7):** a session can turn a server off. Its tools leave `tools` (or the indexed list and `ToolSearch`), and calls to them fail with a tool error saying the server is off. The change applies from the next request, since a changed tool list breaks the prompt cache (§7). The set is kept per session and never saved.

Start with tools, then resources and prompts.

