## 4. Core model

Front ends only ever see these events and send these commands. This boundary is what keeps ACP replaceable.

```rust
pub enum AgentEvent {
    TextDelta(String),
    ThinkingDelta(String),
    ToolStarted { id: ToolCallId, name: String, input: serde_json::Value, title: String },
    ToolFinished { id: ToolCallId, outcome: ToolOutcome },
    PermissionRequest { id: ToolCallId, name: String, input: serde_json::Value,
                        can_always: bool, diff: Option<FileDiff>, preview: Option<Preview>, respond: oneshot::Sender<PermissionReply> },
    Usage { prompt_tokens: u32, used_tokens: u32, context_size: u32, cached_tokens: Option<u32> },
    Compaction(CompactionState),
    Notice { severity: Severity, title: String, detail: Option<String> },
    TurnEnded(StopReason),
}

pub enum StopReason { EndTurn, MaxTokens, MaxTurnRequests, Cancelled, LoopDetected, ProviderError }

pub enum PermissionReply { AllowOnce, AllowAlways, RejectOnce, RejectAlways, Cancelled }
```

`ToolOutcome` is `Completed(ToolOutput)` or `Cancelled`. Variants and fields arrive with the task that first produces them: `Compaction` with T2.6, `diff` with T5.4 (the change a `Write` or `Edit` would make, from `tools::proposed_diff`, computed without writing), `preview` with T5.8. `can_always` says whether a front end may offer "always" (§9); core treats a disallowed `AllowAlways` as once. A `ToolStarted` precedes its `PermissionRequest`, and a denied call finishes as an error `ToolFinished`.

The agent loop runs as one Tokio task per session, sends events on an `mpsc` channel, and observes a `CancellationToken`. Handlers must be `Send`, which ACP also requires.

Before a turn may end, a turn that changed files (a successful `Edit` or `Write`) and ran no `Bash` after the last change gets one note, sent as a `MEMORY_MARK` user message, and the model gets another request. A second such note never comes in the same turn (T13.7).

Map to ACP stop reasons (§11): `LoopDetected` becomes `end_turn` plus a warning `notice`, and `ProviderError` becomes a JSON-RPC error on the prompt.

### Loop

```mermaid
flowchart TD
    turn(["User prompt"]) --> prompt["Assemble prompt (§7)"]
    prompt --> request["Call LM Studio<br/>stream and emit deltas"]
    request --> parse["Parse and recover tool calls (§5.3)"]
    parse --> any{{"Any tool calls?"}}
    any -->|"No"| done(["TurnEnded: EndTurn"])
    any -->|"Yes"| pre

    pre["For each call, in order:<br/>PreToolUse hooks"] --> check{{"Permission check (§9)"}}
    check -->|"Allowed"| run["Execute<br/>sandboxed if Bash"]
    run --> post["PostToolUse hooks"]
    post --> budget["Budget the result (§7)<br/>append to session"]
    check -->|"Denied: error to the model"| budget

    budget --> loopcheck{{"Loop detected? (§5.4)"}}
    loopcheck -->|"Yes"| stuck(["TurnEnded: LoopDetected"])
    loopcheck -->|"No"| compact["Compaction check (§7)"]
    compact --> limit{{"max_turn_requests reached?"}}
    limit -->|"Yes"| capped(["TurnEnded: MaxTurnRequests"])
    limit -->|"No: next request"| prompt

    classDef stop fill:#f3f4f6,stroke:#6b7280,color:#111827
    classDef decide fill:#fef3c7,stroke:#d97706,color:#78350f
    classDef begin fill:#dbeafe,stroke:#2563eb,color:#1e3a8a
    class done,stuck,capped stop
    class turn begin
    class any,check,loopcheck,limit decide
```

A cancel or provider failure can end the turn at any step, giving `TurnEnded(Cancelled)` or `TurnEnded(ProviderError)`.

Tool calls run sequentially, except for one case. Within a round, each run of consecutive read-only calls (built-in `Read`, `Grep` or `Glob`, with no matching `PreToolUse` hook, no MCP name clash, and a permission decision of `Allow` with no prompt) runs concurrently, and the run ends at the first other call. Its outputs are consumed at each call's serial position, so events, loop detection, cancellation and history keep call order, and request bytes match the serial loop. Writes, shell, MCP, subagents and anything that prompts are barriers that run alone, so a read never overtakes a write before it. Local servers process requests serially, and ordering keeps the logs and permission prompts understandable. Cap at `max_turn_requests` (default 50).

