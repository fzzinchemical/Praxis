# VS Code extension checklist

Manual checks for what native tests can't reach (SPEC §14). Install with `cargo xtask vsix` into an isolated profile: `env -i HOME=$HOME PATH=$PATH DISPLAY=$DISPLAY code --user-data-dir <tmp> --extensions-dir <tmp> --install-extension target/harness.vsix`, then open a folder with the same flags.

- [ ] **Open panel, send prompt, see a response (T5.2).** Set `harness.binaryPath` in user settings, run "Harness: Open Chat", wait for `[ready]`, send "Say hi". Reply text streams in and ends with `[end_turn]`.
- [ ] **Restart on exit (T5.2).** Kill the `harness acp` process. The panel shows the exit, an error offers Restart, and Restart brings back `[ready]`.
- [ ] **Output channel (T5.2).** The "Harness" output channel shows the agent's stderr.
