# ADR 0006: Keep building after the OpenCode baseline

> [!NOTE]
> **Status: accepted, 2026-11-12.** Owner decision, closing T0.2 and T2.9.

## Context

T0.2 asked for existing tools to be run on the eval tasks before choosing to build, narrow scope or contribute upstream. It ran late, after M0 to M2 and much of M3 were built. [docs/baseline.md](../baseline.md) has the results: OpenCode passes 8/13 on qwen3.5-4b against the praxis's 6/13, and both pass 10/13 on gemma-4-12b. The praxis is about 4.5× faster on 4b, mostly because it stops loops that OpenCode runs to the time limit. The runs are single-shot, so the pass-rate gaps are within noise.

## Decision

1. **Build.** The project continues as planned. It doesn't contribute upstream instead, and the scope stays the same.
2. **M2 is accepted as it stands (T2.9).** The praxis doesn't clearly beat OpenCode on pass rate. The owner accepts that and moves on to M3 without a rerun with more runs per task.

## Consequences

- The praxis's case rests on speed with small models, enforced safety (sandbox, trust gate, permissions), and being local-first and Rust-only, not on scoring higher on these tasks today.
- The OpenCode numbers in `evals/results/opencode.*.jsonl` are the external baseline for later checkpoints. A fairer rerun (three runs per task, `AGENTS.md` on for both) is still open and needs GPU time.
