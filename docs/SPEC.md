# Implementation spec

The contract for implementing agents. Why the project exists is in [ROADMAP.md](../ROADMAP.md); the order of work is in [TASKS.md](TASKS.md).

> [!NOTE]
> **Evidence labels:** external facts were checked on 2026-11-06 unless marked `UNVERIFIED`, which means from memory or inferred. Confirm those against the live server or docs.rs before relying on them, and correct this file.
>
> **Rust signatures** show intent; names may change, invariants may not.
>
> **Numeric defaults** such as budget percentages and limits are starting points to be tuned through evals, and they all live in config.

## Sections

`§N` in code and docs means section N. Read only the sections your task names.

- [§1. Goals](spec/01-goals.md)
- [§2. Performance](spec/02-performance.md)
- [§3. Workspace](spec/03-workspace.md)
- [§4. Core model](spec/04-core-model.md)
- [§5. Provider](spec/05-provider.md)
- [§6. Tools](spec/06-tools.md)
- [§7. Context management](spec/07-context-management.md)
- [§8. Config, trust and sessions](spec/08-config-trust-and-sessions.md)
- [§9. Permissions and sandbox](spec/09-permissions-and-sandbox.md)
- [§10. Security invariants](spec/10-security-invariants.md)
- [§11. Front ends](spec/11-front-ends.md)
- [§12. Plugins](spec/12-plugins.md)
- [§13. MCP](spec/13-mcp.md)
- [§14. Testing](spec/14-testing.md)
- [§15. Error and logging policy](spec/15-error-and-logging-policy.md)
- [§16. Sources](spec/16-sources.md)
