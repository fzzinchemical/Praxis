# ADR 0005: An orchestrator model delegates to local workers

> [!NOTE]
> **Status: accepted, 2026-11-11.** Owner decision. It builds on [ADR 0004](0004-anthropic-backend-and-per-agent-models.md) and changes none of its decisions.

## Context

ADR 0004 lets each delegated agent run on its own model, but it leaves out who drives the work. The owner wants an agentic workflow: a larger model plans, splits the task and sends commands to local models that do the work. The larger model can be a Claude model on the Anthropic backend, or a larger local model in LM Studio. The workflow must not depend on which one is used.

## Decision

1. **The orchestrator is the session model.** Any model reference works (`anthropic/claude-opus-5-5`, `lmstudio/<large model>` or an alias), so swapping Claude for a large local model is a config change, not a code path.
2. **Workers are agent definitions (SPEC §12) whose `model` names a local model.** The orchestrator delegates through the delegation tool, giving a self-contained instruction. The worker runs its own loop with its own history, profile, context budget and tool limits, and returns a short report.
3. **Only the report enters the orchestrator's history.** The orchestrator never sees the worker's transcript, so a cloud orchestrator receives only what the report contains, and its prompt prefix stays stable.
4. **Delegation stays sequential,** as in ADR 0004. One GPU can't run two local models at full speed.
5. **No praxis-side routing.** The orchestrator model picks which worker to call, and the user picks the models. The praxis never chooses or switches a model itself (ADR 0004, point 6).
6. **Workers get no more than the session allows.** Every worker call goes through the same validation, permission checks, hooks and sandbox. A worker's `tools` can only narrow the parent's set.

## Consequences

- **A built-in orchestrator profile is needed:** an agent definition and prompt that tell the session model to plan, delegate small concrete steps, check each report and verify the result. Workers get a prompt that asks for a short, factual report.
- **A large local orchestrator shares the GPU with its workers.** If both models don't fit in VRAM together, LM Studio swaps them at every delegation, and load time can outweigh the gain. The orchestrator task measures this and records which pairs fit (16 GB VRAM, see AGENTS.md).
- **A cloud orchestrator sends its own context off the machine,** including the reports workers return. The first-use notice and opt-in rules from ADR 0004 apply unchanged. Workers on local models send nothing.
- **This is not a helper model.** SPEC §1 still excludes helper models that compress context or format tool calls for the main model. Here every model runs a full agent loop on its own part of the task.
- **The eval runner needs an orchestrated mode** to show whether orchestrator plus local workers beats a local model alone, and how close it gets to the orchestrator alone.
