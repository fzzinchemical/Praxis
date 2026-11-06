# ADR 0002: LM Studio provider findings

> [!NOTE]
> **Status: accepted, 2026-11-06.** From spike T0.3. Recordings and raw timings are in `crates/harness-core/tests/fixtures/lmstudio/`.

## Context

SPEC §5.2 and §7 left provider behaviour unverified. The owner chose two models: `qwen/qwen2.5-coder-7b-instruct` and `google/gemma-4-12b-qat`. Both were loaded at 8192 context, with one client and an otherwise idle server, using synthetic prompts.

## Findings

**Prompt cache.** The server reuses a shared prefix, and it is the largest latency lever measured. Time to first token, about 5000-token prompt, 3 rounds:

| Model | Cold | Shared prefix | Gain | First character changed | `tools` array changed |
|---|---|---|---|---|---|
| qwen2.5-coder | 7.59 s | 0.14 s | ~54x | 7.55-7.59 s | 7.77 s |
| gemma-4-12b-qat | 7.74 s | 0.41 s | ~19x | 7.74-7.77 s | 7.83 s |

An early change costs the full reprocess, and so does changing `tools`, which confirms the stable-prefix rules on both models. `usage` has no cached-token field, so time to first token is the only signal.

**Tool-call formats.**

| Model | `trained_for_tool_use` | Emitted |
|---|---|---|
| gemma-4-12b-qat | true | Native `tool_calls`, valid JSON, arguments streamed in pieces, `reasoning_content` first. Asked to read two files, it called only the first in the one run inspected |
| qwen2.5-coder | false | Never native. Text in a ```` ```xml ```` fence wrapped in `<tools>` or `<response>`, holding `{"name", "arguments"}`, one object per line for several calls. The multi-line `Write` was invalid JSON in both runs: `{{` plus a single-quoted string |

**Oversized prompts.** No silent truncation in any run. Over the context, the server answers **HTTP 200** and an SSE `event: error` whose message contains `exceed_context_size_error`, `n_prompt_tokens` and `n_ctx`. Largest accepted: 8071 of 8192. Smallest rejected: 8220.

**Constrained output.** `response_format` with a JSON schema gave valid arguments on qwen, with or without `tools`. With both set, neither model produced `tool_calls`: the JSON arrives as `content`. On gemma without tools, reasoning used all 256 output tokens and no JSON appeared, so that case is inconclusive.

**Instances.** `model` accepts an instance id: `<key>:2` reached the 4096-context instance, shown by its `n_ctx` error. With two loaded, the bare key reached the first one (a single observation).

## Decision

- Keep the prompt prefix stable as specified, and use time to first token as the cache signal.
- Treat an SSE `event: error` as a failure even on HTTP 200; `exceed_context_size_error` forces compaction.
- Address models by instance id from `ensure_ready`.
- Constrained retry sends `response_format` for one known tool and reads the arguments from `content`. Allow enough output for reasoning on reasoning models, or turn reasoning off for that request.
- qwen2.5-coder needs a text-call parser for the fenced format and stronger repair. Don't assume `native` from a `qwen*` name.

## Consequences

Native calling is not universal even among the chosen models, so T2.2's recovery is required for qwen rather than an edge case. Not tested: cache eviction, concurrent clients (`parallel` loads as 4), output running past the context limit, real code prompts (the 2.1 chars per token here comes from synthetic text and does not challenge the §7 estimate), and larger contexts.
