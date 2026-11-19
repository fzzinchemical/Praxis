## 2. Performance

Inference dominates wall-clock time, so the priorities are:

1. **Avoid prompt reprocessing.** Keep the prefix stable (§7). This is the largest latency lever by far.
2. **Send fewer tokens.** Budgets, truncation, repo map.
3. **Praxis overhead.** Targets below.

| Metric | Target | Measured by |
|---|---|---|
| `praxis --version` cold start | < 20 ms | T7.2 |
| Ready for input, no MCP servers | < 100 ms | T7.2 |
| Ready for input with MCP servers | Same: servers connect in the background | T7.2 |
| Idle RSS, excluding servers | < 30 MB | T7.2 |
| Stream chunk to emitted event | < 1 ms | T7.2 |

These targets are initial estimates; adjust them if measurements show they are wrong.

Release profile: `lto = "fat"`, `codegen-units = 1`, `strip = true`. Keep `panic = "unwind"` so a panicking tool task becomes a tool error rather than killing the session.

Don't add custom allocators, arenas or `unsafe` optimisations without a benchmark showing a problem.

