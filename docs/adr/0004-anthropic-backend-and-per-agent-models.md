# ADR 0004: Add an Anthropic backend and per-agent models

> [!NOTE]
> **Status: accepted, 2026-11-11.** Owner decision. This replaces the "LM Studio only" backend decision in ROADMAP and SPEC §1. API facts are from Anthropic's docs as of 2026-11-03; confirm them against recorded fixtures (T8.1) before relying on them.

## Context

ROADMAP chose LM Studio as the only backend so that there would be no provider abstraction to maintain. The owner now wants two more things:

- **An Anthropic backend** (the Claude Messages API), for tasks that are too hard for a local model and as a reference ceiling for the eval scoreboard.
- **Different models for different agents.** The session runs on one model, and a delegated agent (SPEC §12, T6.5) can run on another model, on either backend. For example, a local model edits while a Claude model plans or reviews, or the other way round.

Local-first still holds. Nothing leaves the machine unless the user configures it to (ROADMAP).

## Decision

1. **Two backends: LM Studio and Anthropic.** LM Studio stays the default. Anthropic is opt-in and can be enabled only in **user** config or by a flag, never by project config (SPEC §10).
2. **Raw HTTP over the existing `reqwest`, with no SDK crate.** Anthropic has no official Rust SDK, and SSE line parsing already exists (`provider/stream.rs`). This adds no dependency to SPEC §3.
3. **A closed `enum Backend { LmStudio, Anthropic }` inside `Provider`, not a trait.** There are exactly two variants, tests still go through HTTP against `wiremock`, and nothing needs a mock. The agent loop keeps seeing `ChatChunk`, `Usage` and `ProviderError`.
4. **Models are named as `<backend>/<model>`** (`lmstudio/qwen3.5-4b`, `anthropic/claude-sonnet-5-5`), or by an alias from `[models.<alias>]` in config. A bare name means LM Studio, so existing config and commands keep working.
5. **Each agent has its own model.** The session model comes from `--model` or `default_model`. A delegated agent's `model` (for example `sonnet`, `haiku` or an alias) is resolved through `[models]`, and falls back to the session model. Delegation stays sequential, and each agent keeps its own history, profile and context budget.
6. **Automatic routing is still left out.** The user or agent definition chooses the model, and the praxis never switches models on its own.

## Consequences

- **The provider gains a second chat client.** It maps `Message` and `ToolSchema` to the Messages API, and maps SSE `content_block_*` events back to `ChatChunk`. Model management (`ensure_ready`, load and unload) applies to LM Studio only. For Anthropic, context size is read from `GET /v1/models/{id}` (`max_input_tokens`).
- **Thinking blocks are kept, not stripped,** on Anthropic. The API binds thinking blocks to the model and conversation and checks that history isn't edited. So SPEC §5.3's "strip reasoning before storing" and §7's masking must leave Anthropic history append-only, or accept that thinking is dropped. T8.3 measures this, and T8.4 decides.
- **Prompt caching becomes explicit.** One top-level `cache_control` marks the stable prefix, and `usage.cache_read_input_tokens` fills `AgentEvent::Usage.cached_tokens`, which LM Studio never reported. The stable prefix rules in SPEC §7 apply unchanged.
- **Some local-model machinery is off for Anthropic.** Text-embedded call parsing and constrained retry are switched off: current models reject forced `tool_choice`, so tools use `strict: true` instead. Schema validation and permission checks stay on for every call (SPEC §5.3).
- **New request rules.** Requests send `anthropic-version: 2026-10-21`, which rejects `temperature`, `top_p` and `top_k`, so profiles for Anthropic models must not set them. Thinking is `{type: "adaptive"}`, with depth set through `output_config.effort`.
- **New errors** each map to a `ProviderError` with a fix: missing key, 401, 429 or 529 overload (retry after `retry-after`), `stop_reason: "refusal"`, and an SSE `event: error` mid-stream.
- **Cost and privacy.** Every Anthropic request sends prompt and file contents off the machine and costs money. The first use in a session shows a notice naming the model. Evals against Anthropic are never part of verify, and they need human approval to run.
- **Switching models mid-session** invalidates the server's prompt cache. The API silently drops thinking blocks written by another model. Delegated agents have their own histories, so they avoid this.
- **The security surface grows.** SPEC §10 invariant 8 now covers the Anthropic endpoint, and a new invariant stops a repository from turning on a cloud backend.
