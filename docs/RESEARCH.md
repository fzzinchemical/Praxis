# Research findings

What published research says about reducing tokens and supporting developers in the IDE, and what this project does as a result. Researched on 2026-11-06. The resulting requirements are in [SPEC.md](SPEC.md), [TASKS.md](TASKS.md) and [USER_STORIES.md](USER_STORIES.md).

> [!IMPORTANT]
> **The main finding:** most token-reduction results don't transfer directly to this project. They measure cloud pricing, while a local model pays in reprocessing time and context space, on one GPU with no room for a helper model. So this project prefers rule-based methods, rewrites history rarely, and treats every saving as unproven until its own evals confirm it.

**Contents:** [Limits of this evidence](#limits-of-this-evidence) · [Token reduction](#token-reduction) · [Developers in the IDE](#developers-in-the-ide) · [Security](#security) · [Sources](#sources)

---

## Limits of this evidence

- **No study matches this setup.** None of these papers tests a local coding agent on about 16 GB of VRAM. The closest are open-weight models served on far larger hardware.
- **Much of it is recent preprint work.** The [sources table](#sources) gives each one's status. Benchmarks are mostly SWE-bench variants, which differ from daily IDE work.
- **How the papers were read.** Masking and CORVUS details came from extracted PDF text. The other figures came from automated summaries of arXiv pages, so check the paper before relying on an exact number.
- **Not used as evidence:** "When Help Hurts" (ACM blocked access) and the older Copilot studies by Barke, Mozannar and Vaithilingam, where only second-hand summaries were found. Repo-map research gave no usable token figures either, so T2.8 stays gated on evals.
- **Two assumptions are untested.** Whether LM Studio reuses a shared prompt prefix, and whether grammar constraints work alongside tool calling. T0.3 tests both.

---

## Token reduction

### The conflict

| Finding | Source |
|---|---|
| Replacing old tool output with a placeholder halves cost, with a solve rate matching LLM summaries | Complexity Trap |
| Any rewrite of an old turn invalidates the cache from that point on. With cache-aware pricing, none of the delayed-deletion setups was cheaper, and 26 of 42 cost significantly more | Hidden Costs of Forgetting |
| Deleted tool output is fetched again up to 7.5× more often with a delay and 10.6× without, reaching back a median of 6-12 turns | Hidden Costs of Forgetting |
| Deleting immediately hurts: 19 of 42 setups solved significantly fewer tasks, against 5 of 42 with a two-turn delay | Hidden Costs of Forgetting |

So the two pieces of advice conflict: mask old output to save tokens, but avoid rewriting history to keep the cache. On local hardware, a cache miss means waiting while the prompt is reprocessed (our inference: those papers measured cloud billing).

**Decision: mask in rare, batched passes rather than a rolling window.** Masking only starts when the context nears its limit, then frees a large block in one rewrite. Hidden Costs recommends rewriting less often but didn't test this exact design, so the evals must prove it (SPEC §7).

### What we do with each technique

| Technique | Evidence | Decision |
|---|---|---|
| **Observation masking** | About 50% lower cost at a matching solve rate. Window size needs tuning per agent (M=10 for SWE-agent, M=58 for OpenHands). A small model lost a little: Qwen3-32B without thinking went from 17.0% to 15.0%, with overlapping intervals | ✅ **Adopt, batched.** First stage of compaction. Tune M with evals |
| **LLM summaries** | No better than masking, and the calls themselves were up to 7.2% of cost | ✅ **Keep as the second stage,** only if masking isn't enough |
| **Stable prefix** | Caching cut cost 41-80% and time to first token 13-31% on cloud providers. Dynamic content belongs at the end, and dynamic tool lists hurt | ✅ **Already planned.** Validated |
| **Frozen tool list with search** | RAG-MCP reports over 50% fewer prompt tokens, and 43.13% vs 13.62% tool selection accuracy. Its own numbers, and a community replication found a small accuracy loss instead | ✅ **Already planned,** with lexical search, since embeddings need a second model. Don't expect the accuracy gain |
| **Search-replace edits** | Best format for larger models. Small open models gain little from any format | ✅ **Already planned.** No change |
| **Removing waste from tool output** | AgentDiet removes useless, redundant and expired content: 39.9-59.7% fewer input tokens and 21.1-35.9% lower cost. It uses a second LLM, and its authors doubt regex rules would cover enough | 🔶 **Adapt with rules only,** applied once when output is first recorded. Expect smaller savings than the paper. Keep only if evals agree |
| **One current copy of each file** | CORVUS: 9-50% fewer input tokens and up to 37% fewer cycles. Its authors note the synced block limits cache reuse | 🔶 **Adapt.** Repeat reads of unchanged files return a short stub, and stale files are flagged. This is our lighter variant, not what CORVUS tested |
| **Constrained decoding** | Schema validity rose from 78.6-92.9% to 100% on 0.6-4B models. Multi-step function-calling errors remained. A short single-author preprint | 🔶 **Adapt** as a retry after a malformed call, if T0.3 shows LM Studio supports it |
| **Prompt compression (LLMLingua)** | About 10% reduction on docstrings before quality fell. In CoACT's comparison, LLMLingua-2 dropped to 50.0% from 57.0% and used more tokens | ❌ **Reject.** Harmful on code, and needs a second model |
| **Code minification** | 42% fewer tokens, but resolved tasks fell from 50.0% to 38.0% | ❌ **Reject.** Rewritten content would also break `Edit`'s exact matching |
| **Learned compressors** | CoACT: 33.0% fewer tokens with stable results, but it needs a trained model and a teacher | ❌ **Reject.** One GPU can't spare it |
| **Attention-guided compression** | AttnCompress: 21.6% fewer tokens and 33.6% lower cost. We couldn't confirm that LM Studio exposes the signals it needs | ⏳ **Watch** |
| **Helper model for tool calls** | IFFC claims better function calling, but the page gave no figures | ❌ **Reject for now.** Same GPU constraint |

**Measuring it:** Hidden Costs warns that token counts alone overstate savings. So the scoreboard reports turns, wall time, re-fetches and tokens processed versus cached, against an uncompressed baseline (SPEC §14).

---

## Developers in the IDE

### What the studies show

| Finding | Source | Consequence |
|---|---|---|
| Developers working incrementally and iterating on results did better than one-shot users. Only about half of the issues were resolved | Kumar et al. | Make plan-first and stopping to redirect easy |
| Multi-file review is mainly a trust problem, not a diff problem. Chunks were the most valued idea, and the workflow runs overview, then file, then chunk | Heander et al. | Add a change-set review |
| Developers couldn't tell secure suggestions from insecure ones, and only 22% of submissions were fully secure. Effort fell as tasks went on: test runs dropped from 5.8 to 3.8 | Khalid et al. | Don't rely on approvals alone, and keep prompts rare and meaningful |
| Assistant users wrote less secure code and rated it as more secure | Perry et al. | Never show "looks safe" signals |
| Developers were 19% slower with AI in early 2025 but believed they were 20% faster. METR now calls that result out of date, with later data suggesting small gains of about 4-20%, and weak evidence | METR | Measure time on task, not perception |
| Proactive suggestions passed 12-18% more test cases, but a persistent variant was preferred by only 47% against 90%. Short tasks, mostly students | Chen et al. | Summary first, details on demand. Avoid constant suggestions |
| Presence indicators and interaction context reduced the disruption caused by proactive agents | Pu et al. | Always show what the agent is doing |
| The interaction loop has four parts: task alignment, verifiability, steerability, adaptability | Wang et al. (position paper) | Used as a checklist below |

### What we build

| Need | Feature | Priority | Tasks |
|---|---|---|---|
| **Task alignment** | Plan-first flow: propose a plan in read-only mode, then switch modes to carry it out | Should | T5.9 |
| **Verifiability** | Turn summary built from the tool log, not the model's claims: files changed, commands run, exit codes, and what wasn't run | Should | T5.7 |
| **Verifiability** | Change-set review: overview, then file, then chunk, with free navigation and revert per file | Should | T5.8 |
| **Steerability** | Stop at any moment and continue with a new instruction, keeping history consistent | Must | T1.7 |
| **Awareness** | Status line with phase, elapsed time and context used, which matters because local models are slow | Must | T5.3 |
| **Attention** | Collapsed tool output with a one-line summary | Must | T5.3 |
| **Honest behaviour** | System prompt asks the model to state assumptions, disagree when warranted, and report only what it verified | Should | T2.13 |
| **Evidence** | Timed trial of real tasks with and without the harness | Could | T7.5 |

Risk labels are rule-based (paths such as CI, dependencies and auth). They only direct attention, and are never shown as a safety verdict.

**Deliberately not built:**
- **Proactive suggestions.** The benefit is real, but they keep inference running on the only GPU, and the evidence comes from short student tasks.
- **Automatic LLM review verdicts.** Survey support is only directional, and a confident verdict risks the overconfidence Perry et al. found. A user can still run a review as a plugin skill.

---

## Security

| Finding | Source |
|---|---|
| Tool poisoning succeeded in 22.9% of cases across 12 frameworks and 18 models, and no framework had a deliberate defence | Shaikh & Montaruli |
| Susceptibility varies widely among open-weight models: Llama 4 Maverick 44.1%, DeepSeek-V3.2 44.0%, Gemma 3 27B 4.1%, DeepSeek-R1 3.6%. These ran through a cloud service, not locally | Shaikh & Montaruli |
| A regex description scanner stopped 20 of 45 scenarios and falsely flagged 7.2% of benign descriptions, so it is only a partial control | Shaikh & Montaruli |
| In most clients, protection came from model refusals rather than the client. Common gaps were limited parameter visibility and missing sandboxing | Huang et al. |

**Decisions:**
- **Pin MCP tool definitions (new, Must).** Hash each tool's name, description and schema on approval, and require fresh approval if they change.
- **Scan descriptions, but only to warn.** The false-positive rate rules out blocking on it.
- **Always show full parameters in permission prompts.**
- **Report injection results per model on the scoreboard,** because you can't assume a local model will refuse.

---

## Claude Code's markdown files

Checked 2026-11-12 against the [memory](https://code.claude.com/docs/en/memory.md), [skills](https://code.claude.com/docs/en/skills.md) and [subagents](https://code.claude.com/docs/en/sub-agents.md) docs. These formats belong to Claude Code and change often; re-check before implementing.

| Area | Claude Code | Harness ([ADR 0007](adr/0007-markdown-sources-like-claude-code.md)) |
|---|---|---|
| Memory locations | Managed, `~/.claude/CLAUDE.md`, every ancestor of cwd up to `/`, `CLAUDE.local.md`; subdirectories lazily | Same, except no managed file and nothing above the workspace root |
| `AGENTS.md` | Read only when no `CLAUDE.md` exists | Same |
| Imports | `@path`, four hops, skipped in code; outside-cwd imports need approval | Same syntax and depth; outside imports are skipped |
| Rules | `.claude/rules/**/*.md`, `paths` globs | Same, plus `~/.config/harness/rules/` and `.harness/rules/` |
| Skills and commands | Merged namespace; enterprise > personal > project; many frontmatter fields, `` !`cmd` `` injection, `context: fork` | Same namespace and order; subset of fields; no injection or fork |
| Agents | Managed > CLI > project > user > plugin; `permissionMode`, `hooks`, `mcpServers`, `memory`, `isolation` and more | Project > user > plugin; §12 fields only |
| Settings | `permissions`, `hooks` and many more in `settings.json` | `permissions` and `hooks` only; project ones through the trust gate |

Not confirmed: whether local Claude Code expands `@file` inside skill bodies (docs only mention it for synced skills), so the harness doesn't.

## Sources

Status is as listed on each paper's page on 2026-11-06.

| Short name | Paper | Status |
|---|---|---|
| Complexity Trap | [Lindenbauer et al., The Complexity Trap](https://arxiv.org/abs/2508.21433) | NeurIPS 2025 workshop (DL4C) |
| Hidden Costs of Forgetting | [Vallecillos-Ruiz et al.](https://arxiv.org/html/2610.35496) | Preprint |
| AgentDiet | [Xiao et al., Reducing Cost of LLM Agents with Trajectory Reduction](https://arxiv.org/abs/2509.23586) | Accepted, FSE 2026 |
| CORVUS | [Zheng et al.](https://arxiv.org/abs/2607.22711) | Preprint |
| CoACT | [Chen et al.](https://arxiv.org/html/2607.02911v1) | Preprint |
| AttnCompress | [Zeng et al.](https://arxiv.org/abs/2609.08318) | Accepted, ISSTA 2026 |
| Don't Break the Cache | [Lumer et al.](https://arxiv.org/abs/2601.06007) | Preprint |
| RAG-MCP | [Gan & Sun](https://arxiv.org/abs/2505.03275) | Preprint |
| Diff-XYZ | [Glukhov et al.](https://arxiv.org/abs/2510.12487) | Preprint |
| Constrained decoding | [Chavan](https://arxiv.org/abs/2609.23742) | Preprint |
| DocString compression | [Yang et al., Less is More](https://arxiv.org/abs/2410.22793) | TOSEM, per the page's comments |
| Minification | [Hrubec & Cito](https://arxiv.org/html/2606.01326v1) | Preprint |
| IFFC | [Taheri et al.](https://arxiv.org/abs/2608.22472) | Preprint |
| Kumar et al. | [Why AI Agents Still Need You](https://arxiv.org/abs/2506.12347) | Accepted, ASE 2025 (19 developers, 33 issues) |
| Heander et al. | [Trust-Calibrated Code Review](https://arxiv.org/abs/2606.01969) | Submitted, ESEM SEIP 2026 (17 practitioners, 43 surveyed) |
| Khalid et al. | [(Don't) Trust, but (Don't) Verify](https://arxiv.org/abs/2609.21020) | To appear, IEEE S&P 2027 (100 participants) |
| Perry et al. | [Do Users Write More Insecure Code with AI Assistants?](https://arxiv.org/abs/2211.03622) | ACM CCS 2023, per search results (47 participants) |
| METR | [Early-2025 study](https://metr.org/blog/2025-07-10-early-2025-ai-experienced-os-dev-study/), [2026 update](https://metr.org/blog/2026-02-24-uplift-update/), [risk report](https://metr.org/blog/2026-05-19-frontier-risk-report/) | Research organisation (16 developers, 246 tasks in 2025) |
| Chen et al. | [Need Help?](https://arxiv.org/abs/2410.04596) | CHI 2025 (65 participants) |
| Pu et al. | [Assistance or Disruption?](https://arxiv.org/abs/2502.18658) | ACM, 2025; venue not confirmed (18 participants) |
| Wang et al. | [Humans are Missing from AI Coding Agent Research](https://arxiv.org/abs/2608.12355) | Position paper |
| Shaikh & Montaruli | [MCP Tool Poisoning in Agentic AI Frameworks](https://arxiv.org/html/2611.03555v1) | Prepared for AISec 2026 |
| Huang et al. | [Are AI-assisted Development Tools Immune to Prompt Injection?](https://arxiv.org/html/2603.21642v1) | Preprint |
| LM Studio | [Structured output](https://lmstudio.ai/docs/developer/openai-compat/structured-output), [tool use](https://lmstudio.ai/docs/developer/openai-compat/tools) | Vendor docs |
