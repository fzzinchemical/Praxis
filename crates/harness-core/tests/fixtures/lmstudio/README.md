# LM Studio recordings (T0.3)

Raw server responses from `record.py`, recorded 2026-11-06. `*.sse` and `*.response.json` are byte-exact, including malformed model output: never hand-edit them; re-record instead. In `*.request.json`, prompts over 2000 characters are replaced by their size. `results.json` holds per-request timings and usage. Findings are in [ADR 0002](../../../../../docs/adr/0002-lmstudio-provider-findings.md).

Re-record with a running server (this loads and unloads models): `mkdir out && python3 record.py out`. The script is a dev tool, not product code.
