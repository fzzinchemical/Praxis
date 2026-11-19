## 15. Error and logging policy

- `praxis-core` returns typed errors (`thiserror`). Only the CLI top level uses `anyhow`.
- Users get what happened and what to do. Stack traces are for `RUST_LOG=debug` on stderr.
- Logs exclude prompts, file contents and secrets at `info` and below. Debug-level content logging needs an explicit `PRAXIS_LOG_CONTENT=1`.
- A task panic is caught at the tool boundary and becomes a tool error. A session log write failure stops the turn, since continuing would lose data.

