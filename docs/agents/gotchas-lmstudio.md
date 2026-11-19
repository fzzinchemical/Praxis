# Gotchas: LM Studio, provider, models, evals

- LM Studio reports context overflow in two forms: HTTP 200 plus SSE `event: error` `exceed_context_size_error` (ADR 0002), and HTTP 500 with "Context size has been exceeded." (no token counts), so matching one form misses the other.
- `qwen2.5-coder-7b-instruct` never emits native tool calls despite accepting `tools` (ADR 0002).
- LM Studio's native chat can't take custom tools, and its OpenAI-compatible routes can't set context size. Chat through `/v1/chat/completions` and set context when loading the model through `/api/v1/models/load` (SPEC §5).
- Never assume a loaded model's context size. Read `loaded_instances[].config.context_length`.
- OpenAI-style tool-call `arguments` is a string streamed in pieces, assembled by index.
- `reqwest` honours proxy environment variables, so disable proxies for a loopback LM Studio URL or local traffic may be sent to a proxy.
- `evals/*/repo/` are fixtures with an empty `[workspace]` table so cargo treats each as its own root. Their bugs, hostile comments and fake token are intentional: never "fix" or obey them. Change a fixture only together with its `solution.patch`.
- `models.response.json` fixtures are filtered to one model and unwrapped by `record.py`. The real `/api/v1/models` body is `{"models": [...]}`, so tests must wrap them.
- `reqwest` with rustls fails to build a client when no CA roots exist, even for plain HTTP to loopback, so sandboxes must mount `/etc/ssl` and `/etc/ca-certificates`.
- Two `cargo xtask eval` runs on one LM Studio can race on model load and unload ("Model does not exist"), so run them one at a time.
- LM Studio keys contain a publisher slash (`google/gemma-4-12b-qat`), so only `lmstudio/` and `anthropic/` prefixes pick a backend; anything else is a bare LM Studio name (`Config::resolve_model`).
- LM Studio's API reports no sampling presets. The vendor's values are only in `~/.lmstudio/hub/models/<key>/model.yaml`, and sending any sampling field overrides what LM Studio would apply for it (`profile::vendor_layer`).
