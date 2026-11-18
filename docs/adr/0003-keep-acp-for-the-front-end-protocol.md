# ADR 0003: Keep ACP as the front-end protocol

> [!NOTE]
> **Status: accepted, 2026-11-06.** From spike T0.5, against `agent-client-protocol` 5.0.0 and `agent-client-protocol-schema` 1.15.1. The spike was `crates/praxis-cli/tests/acp_spike.rs`, deleted once T5.1 replaced it with real ACP tests; see git history (`git log --all -- crates/praxis-cli/tests/acp_spike.rs`).

## Context

ROADMAP chose ACP, with our own JSON-RPC as the fallback if the spike found ACP too unstable. SPEC §11 left parts of the schema unverified.

## Findings

- **It works with little code.** A throwaway agent of about 65 lines echoes a prompt behind one `session/request_permission`, tested against an in-process client with no adapter: a `Builder` is itself a transport for `connect_with`.
- **Every §11 method and update exists in stable v1**, with no `unstable_*` feature: `session/load`, `session/set_mode`, `session/cancel`, `fs/read_text_file`, `usage_update`, `notice`, `compaction_update`, and all five stop reasons.
- **Ordering suits us.** An update sent before the prompt response reached the client first in the test, which is what §11 cancellation needs.
- **One real trap.** Handlers run inside the connection's dispatch loop, so awaiting a client response in one deadlocks. The prompt handler must spawn its work. The crate documents this, and the agent loop has to run as its own task anyway.
- **Schema surprises are small.** See §11: an extra `switch_mode` tool kind, and `mcpServers` being required on the wire.

## Decision

Keep ACP. Don't write our own JSON-RPC. Use protocol v1 only, without `unstable_*` features.

## Consequences

- Zed and JetBrains stay within reach, and the wire types aren't ours to maintain.
- The crate pins its schema crate exactly and moves quickly, so upgrades need care. `schema_wire_format_matches_spec` fails if the wire facts in §11 change.
- ACP stays behind the event layer (ROADMAP), so replacing it later remains possible.

Not tested: the `Stdio` transport and a real editor (T5.1, T5.6), `session/cancel` and `session/load` round trips (T5.1), and the crate on wasm (T0.6 decides between the full crate and schema types only).
