# Gotchas: VS Code, wasm, ACP, CLI

- The `wasm-bindgen` CLI version must exactly match the `wasm-bindgen` crate version. Pacman ships CLI 0.2.128 and crates.io has 0.2.130, so pin the crate to the CLI.
- The webview glue uses `wasm-bindgen --target deno` because it self-starts; `web` needs a banned init script. A broken webview shows as an empty panel, so check it after any `wasm-bindgen` bump (ADR 0001).
- `code` launched from a VS Code terminal inherits `ELECTRON_RUN_AS_NODE` and `VSCODE_*`. Use `env -i` for an isolated test profile.
- ACP handlers run inside the dispatch loop. Awaiting a client response in one deadlocks, so spawn the work and keep the responder (SPEC §11).
- VS Code's IPC socket sits under `--user-data-dir`, and a Unix socket path is capped at 108 bytes, so `vscode-smoke` puts the profile in a short temp dir. `code` must run with `--wait`, or `xvfb-run` kills the display as soon as the CLI returns.
- clap's `args_conflicts_with_subcommands` stops subcommands parsing after any flag, including global ones.
- Once the CLI starts its terminal reader, it owns stdin: later prompts must read its channel (`terminal_approves` uses `block_in_place`), never `stdin().read_line`.
- The ACP crate rejects `session/new` sent before the `initialize` answer arrives, so clients and tests must wait for it. `Stdio` needs the crate's `stdio` feature.
- In wasm-bindgen 0.2.128 a `getter` on a namespaced free function (`workspace.workspaceFolders`) is emitted as a call and throws. Bind the namespace with `thread_local_v2` and read the property with `Reflect::get` (`praxis-vscode/src/lib.rs`).
- In `wasm32-unknown-unknown`, `Path::is_absolute()` is always false (the target is neither unix nor windows). Use `has_root()` in the extension crates; native tests cannot catch this.
- In `agent-client-protocol` 5.0.0, a `cx.spawn` task is dropped when its connection ends, so a running task never sees a `send_notification` error. Don't write tests that expect it to (docs/AUDIT.md F1).
- Default SIGTERM skips destructors, so ACP and REPL race their work against it (`until_sigterm`); a plain `async` main leaks MCP children and tool process groups when VS Code stops it.
- An extension host restart (an extension auto-update) leaves chat webviews open but never hands them to the new host; a panel serializer does not help (it runs on window reload only). `recover_orphans` closes them and reopens their sessions from `workspaceState`.
- A `session/load` replays agent text, which sets the panel to Generating; the `ready` notice must idle the panel, or every prompt queues.
- Webview buttons answer through one delegated click listener on the transcript (`output`); an element appended elsewhere (e.g. `body`) never gets its clicks.
