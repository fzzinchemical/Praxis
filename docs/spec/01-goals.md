## 1. Goals

In priority order:

1. **Reliable on local models.** Task success on small context windows and imperfect tool calling.
2. **Safe by construction.** Permissions and sandboxing hold even if the model is compromised.
3. **Fast.** See §2.
4. **Compatible.** MCP, ACP and Claude Code's plugin layout work unmodified where supported.

Out of scope: hosting models, any model backend other than LM Studio and Anthropic ([ADR 0004](../adr/0004-anthropic-backend-and-per-agent-models.md)), automatic model routing, helper models for compression or tool calling (one GPU, see [RESEARCH.md](../RESEARCH.md)), plugin marketplace, RAG, parallel subagents, telemetry, Windows and macOS sandboxing.

