# VS Code extension checklist

Manual checks for what native tests can't reach (SPEC §14). Install with `cargo xtask vsix` into an isolated profile: `env -i HOME=$HOME PATH=$PATH DISPLAY=$DISPLAY code --user-data-dir <tmp> --extensions-dir <tmp> --install-extension target/harness.vsix`, then open a folder with the same flags.

- [ ] **Open panel, send prompt, see a response (T5.2).** Set `harness.binaryPath` in user settings, run "Harness: Open Chat", wait for `ready`, send "Say hi". Reply text streams in as formatted markdown and ends with "Turn ended: end_turn".
- [ ] **Restart on exit (T5.2).** Kill the `harness acp` process. The panel shows the exit, an error offers Restart, and Restart brings back `ready`.
- [ ] **Output channel (T5.2).** The "Harness" output channel shows the agent's stderr.
- [ ] **Status line (T5.3).** During a turn the line under the transcript shows the phase (Processing prompt, Generating, Running tool), a ticking elapsed time and `context used / size (n%)`; after the turn it shows Idle with the final time.
- [ ] **Tool output (T5.3).** Ask for a file read. The tool shows as a collapsed item with its status; expanding it shows the output.
- [ ] **Permission prompt (T5.3).** Ask for a shell command. A prompt with Allow/Reject buttons appears and takes focus; Allow runs it, Reject refuses it, and the prompt disappears after the answer.
- [ ] **Stop (T5.3).** Start a long reply and press Stop. It ends with "Turn ended: cancelled", an open permission prompt disappears, and the next prompt works.
- [ ] **Unsafe model text (T5.3).** Ask the model to repeat `<script>alert(1)</script> [x](javascript:alert(1))` verbatim. The script shows as text, `x` is not a link, and the webview devtools console shows no CSP violation from model text.
- [ ] **Unsaved buffer read (T5.5).** Open a file, type an edit without saving, and ask "read a.txt and quote its first line". The tool output shows the unsaved text, not the disk text. Then ask it to read `/etc/hostname`; the tool reports an error and the file is not read through the editor.
- [ ] **Keyboard-only pass (T5.3).** Without the mouse: Tab reaches the transcript, prompt, Send and Stop; Enter on Send sends; tool items open with Enter or Space; permission buttons answer with Enter; focus is always visible.
- [ ] **Edit review (T5.4).** Ask the model to change a line in an existing file with `Edit`, then to create a new file with `Write`. Each opens VS Code's diff editor with the old text on the left and the proposed text on the right, and the file on disk is still unchanged. Reject the first: the file is byte-identical, and the model's next reply says it was rejected. Allow the second: the file exists with the proposed content.
- [ ] **Active file chip (T5.5).** Open a file in the workspace: the composer shows a chip with its name. Select some lines and ask "what does the selection do?". The model's reply refers to the selected lines. The chip's × drops the file from the next prompt, and opening another file brings its chip back.
- [ ] **Add file (T5.5).** Press "Add file": VS Code's picker opens at the workspace folder. Pick a file inside it and it appears as a chip. Pick one outside the workspace and the transcript says it was not attached.
- [ ] **Include problems (T5.5).** With a file that has errors open, tick "Include problems" and ask "why does this file fail?". The model names the reported lines; with the box unticked it does not see them.
