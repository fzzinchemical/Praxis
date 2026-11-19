# Models and dev environment

### Models

The chosen models for spikes, baselines and evals are `google/gemma-4-12b-qat` and `qwen3.5-4b`. `qwen2.5-coder-7b-instruct` was tried and dropped (ADR 0002).

### Dev environment

Checked 2026-11-06. Re-check before relying on it.

- CachyOS (Arch-based), kernel 7.3, Landlock enabled, `bwrap` present.
- `cargo`/`rustc` 1.99.1 from the system package. **There is no rustup**, so targets come from pacman.
- AMD Navi 21 GPU (16 GB VRAM on the RX 6800/6800 XT/6900 XT cards using this chip), 62 GiB RAM. No `nvidia-smi`. Expect ROCm or Vulkan, and choose models accordingly.
- VS Code 1.145.0 (`code`). Node exists on the machine, but the project must not depend on it.
- LM Studio with its server on port 1234 (`~/.lmstudio/bin/lms`, not on `PATH`), `rust-wasm`, and the `wasm-bindgen` CLI 0.2.128.
