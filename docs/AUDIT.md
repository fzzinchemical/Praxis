# Code audit 2026-11-13

A checklist for agents. Work one item per branch, as `AGENTS.md` says: write the "Done when"
test first and watch it fail, then fix, run `./scripts/verify`, and tick the box here in the same
branch. Items are numbered by priority; the number stays fixed even after an item is ticked.

**What this repo does:** `harness` is a local coding agent written in Rust. It talks to a model
(LM Studio, or the Anthropic API if the user opts in), and lets the model read, write and edit
files and run shell commands in one workspace. The commands run in a Landlock sandbox and go
through permission checks. It runs as a CLI and, through ACP (Agent Client Protocol: JSON
messages over stdin/stdout), as a VS Code extension.

**Load assumed:** one developer on one machine. There can be several sessions at once: the CLI
and VS Code, several chat tabs, and evals. The VS Code process lives for days. The real threat is
hostile content: model output, repository files and tool results are all untrusted (SPEC §10).

---

## Must fix

- [x] **1. The model can plant code that git runs later, past the `.git` hard denial**
  (`crates/harness-core/src/tools/confine.rs:59-66`)
  - **What this is:** `check_writable` decides which paths `Write` and `Edit` may never touch.
    SPEC §10 wants this to stop the agent from "planting code that runs later".
  - **Problem:** It blocks only `.git/hooks` and `.git/config` when they come directly after a
    `.git` folder. Two cases get through:
    (a) In a repository with submodules, `.git/modules/<name>/hooks/pre-commit` and
    `.git/modules/<name>/config` are not blocked, because the folder before `hooks` is `<name>`
    and not `.git`.
    (b) In a git worktree, `.git` is a regular file (`gitdir: …`). `Write` may replace it and
    point it at a folder inside the workspace. That folder can then hold a `config` with
    `core.fsmonitor = <command>`.
    In both cases the user's next `git status` or `git commit` runs that command, with no
    sandbox. Auto-edit mode never asks first. Ask mode shows only a normal write prompt.
  - **Fix:** Refuse every path that has a component named `.git`: the `.git` file itself and
    anything under a `.git` folder. `Write` and `Edit` never need to write there. This is a
    one-line change to the `denied` expression. Update the SPEC §10 list to match.
  - **Done when:** A test in `crates/harness-core/tests/security.rs` shows that `Write` to
    `.git/modules/sub/hooks/pre-commit`, to `.git/modules/sub/config`, and to a `.git` file
    (the worktree case) all return `HardDenied`.
  - **If we skip it:** Text injected into any file the model reads can lead to code running on
    the user's machine, outside the sandbox.

- [x] **2. In a resumed session, every `Write` and `Edit` fails**
  (`crates/harness-core/src/tools/checkpoint.rs:54`)
  - **What this is:** Before each change, the agent copies the old file to
    `sessions/<id>/checkpoints/<n>` so that `/undo` can restore it. `n` counts up from
    `saved + 1`.
  - **Problem:** `saved` starts at 0 in each process. The CLI (`--resume`) and ACP
    (`session/load`) both reuse the same checkpoint folder. If the earlier run saved a copy,
    file `1` already exists. `create_new` then fails, and so `Write`/`Edit` returns
    "Can't checkpoint …: File exists". The agent can't edit files in that session again.
  - **Fix:** In `checkpoint_into`, start `saved` above the highest number already in the
    folder. Or name each copy with a value that is unique per process, as `new_temp_path`
    already does (pid + counter).
  - **Done when:** A test makes one `ToolContext` that edits a file with a checkpoint folder.
    A second `ToolContext` then uses the same folder (a resume), and its `Edit` succeeds.
  - **If we skip it:** Resuming a session is broken for any session that edited files.

- [x] **3. A resumed session can carry tool calls without results, which breaks the next request**
  (`crates/harness-core/src/agent/mod.rs:516-520`, `crates/harness-core/src/session.rs` `resume`)
  - **What this is:** `with_session` loads the saved messages back into the history.
  - **Problem:** The log is written one message at a time. Suppose the process dies while a tool
    runs: the terminal closes during a long `cargo build`, or VS Code reloads. Then the log
    ends with an assistant message whose `tool_calls` have no `Tool` results, or only some.
    After a resume, the next request breaks the call/result pairing that OpenAI-style chat
    templates require. The server rejects it, or the model gets confused.
  - **Fix:** In `with_session`, after loading `prior`, add a `Message::Tool` with
    `NOT_RUN_RESULT`-style text for each call in the last assistant message that has no result.
    Write those messages to the log too, so the log is repaired as well.
  - **Done when:** A test in `crates/harness-core/tests/session.rs` or `agent.rs` resumes a
    log that ends in an unanswered tool call. The test shows that the next request has a tool
    result for every call id.
  - **If we skip it:** The sessions most likely to need a resume (crashed ones) can't continue.

- [x] **4. Hooks see relative paths, so a guard hook can be bypassed**
  (`crates/harness-core/src/agent/mod.rs:1083-1100`)
  - **What this is:** `PreToolUse` hooks get `tool_input` as JSON. Claude Code hooks often guard
    paths, for example "block writes under `$CLAUDE_PROJECT_DIR/secrets`". SPEC §6 says:
    "Paths are absolute in `tool_input`. Resolve relative paths against the workspace before
    hooks see them."
  - **Problem:** The hook gets `file_path` (and `path` for Grep/Glob) exactly as the model
    wrote it. A hook that checks for an absolute prefix lets `secrets/key.pem` or
    `./x/../secrets/key.pem` through. The tool then writes there anyway.
  - **Fix:** Before `pre_tool_hooks`, for `Read`/`Write`/`Edit` (`file_path`) and `Grep`/`Glob`
    (`path`), replace the value with the result of `confine::resolve` when it resolves. Give
    the tools the same rewritten input, so the hook and the tool see the same path.
  - **Done when:** A test runs a `PreToolUse` hook that echoes `tool_input.file_path`, with a
    relative path. The test shows that the hook received the absolute, resolved path.
  - **If we skip it:** Users who copy their Claude Code guard hooks get less protection than
    they expect.

- [x] **5. `Edit` refuses every file that has unsaved changes in VS Code**
  (`crates/harness-core/src/tools/files.rs:94` vs `:237`)
  - **What this is:** In ACP mode, `Read` returns the editor buffer, unsaved edits included,
    and remembers that text's hash. `Edit` checks that the file hasn't changed since the read.
  - **Problem:** `Edit` hashes the file on disk, not the buffer. With unsaved changes the two
    hashes never match. So `Edit` always says "changed since you read it. Read it again".
    Reading again gives the same buffer, so the model loops until the loop detector stops the
    turn. Even when the hashes match, `Edit` writes to disk under a buffer that differs from
    it.
  - **Fix:** The smallest safe fix: in `Edit`, if the editor buffer differs from the disk,
    refuse with a clear message ("<file> has unsaved changes in the editor; ask the user to
    save it"). Proper buffer edits through `fs/write_text_file` are a separate feature.
  - **Done when:** A test in `crates/harness-cli/src/acp.rs` (next to
    `read_returns_the_unsaved_buffer_when_the_client_serves_files`) reads a dirty buffer, then
    calls `Edit`. The test gets the "unsaved changes" message, not the "Read it again" message.
  - **If we skip it:** In VS Code, the agent can't edit the file the user is working on, and
    it wastes turns trying.

- [x] **6. A hook that doesn't read its input can hang the agent forever**
  (`crates/harness-core/src/hooks.rs:246-248`)
  - **What this is:** `hooks::run` writes the JSON input to the hook's stdin, then waits for the
    hook, with a timeout.
  - **Problem:** The stdin write happens before the timeout starts. If the input is larger than
    the pipe buffer (64 KiB, easy with a large `Write` content) and the hook doesn't read stdin
    (for example `sleep 60; exit 0`, or a hook stuck on the network), `write_all` blocks
    forever. The 30 s timeout never applies.
  - **Fix:** Put the stdin write and `wait_with_output` together inside the one
    `tokio::time::timeout`. When the timeout fires, the child is dropped, and `kill_on_drop`
    already kills it.
  - **Done when:** A test in `hooks.rs` runs a hook `sleep 5` with a 1 s timeout and a 1 MiB
    input. The test gets a "timed out" error within about 1 s.
  - **If we skip it:** One bad plugin or settings hook freezes the session, and the only way
    out is to kill the process.

## Should fix

- [x] **7. Hook events other than `PreToolUse` are dropped without a word**
  (`crates/harness-core/src/hooks.rs:12`, `crates/harness-core/src/agent/mod.rs:282`)
  - **What this is:** `hooks::parse` accepts `PostToolUse`, `UserPromptSubmit`, `SessionStart`
    and `Stop`. Then `Agent::with_hooks` drops all of them except `PreToolUse`. In
    `PreToolUse` results, `continue: false`, `additionalContext` and `systemMessage` are
    parsed and then ignored as well.
  - **Problem:** SPEC §12 says "warn clearly about anything skipped; never fail silently".
    Suppose a user's `~/.claude/settings.json` has a `PostToolUse` formatter or a `Stop` check.
    It silently never runs, and the user believes it does.
  - **Fix:** Until these events are built, have `parse` add a warning for each one ("hooks:
    `<event>` doesn't run yet, skipped"), and remove those events from `EVENTS`. Remove the
    `Outcome` fields that nothing reads (`updated_tool_output`, `context`, `system_message`,
    `stop`), or warn when a hook sets them.
  - **Done when:** The `parse_skips_and_warns_on_unsupported_parts` test also expects a
    warning for a `PostToolUse` entry.
  - **If we skip it:** Users trust hooks that don't run.

- [x] **8. Two sessions saving approvals at once can corrupt `trust.json`, after which nothing starts**
  (`crates/harness-core/src/trust.rs:66`, `crates/harness-core/src/plugin.rs:862-866`)
  - **What this is:** `TrustStore::set` writes `trust.json.partial`, then renames it over the
    store. `Store::save` writes `installed.json` in place with `fs::write`.
  - **Problem:** The temp name is fixed. Two processes (the CLI and VS Code, or two chat tabs
    pinning MCP tools at startup) can both truncate and write the same `.partial` file, then
    rename it. That can leave mixed, invalid JSON. A broken store is a hard error by design,
    so every later start fails with "trust store: …" until the user deletes the file. In the
    same way, a crash during `Store::save` leaves a half-written `installed.json`, and then
    every plugin is skipped. Both are also read, changed and written back with no lock, so
    one of two writes can be lost (the safe direction: it only asks again).
  - **Fix:** Reuse `tools::files::replace_with` (it uses a unique temp name, `create_new`,
    `fsync` and rename) for `TrustStore::set`, `Store::save` and `Plugin::set_option`. Expose
    it as `pub(crate)`. That deletes the hand-written temp-file code in `trust.rs`.
  - **Done when:** A test runs `TrustStore::set` from many threads on one store and then reads
    valid JSON.
  - **If we skip it:** A rare race locks the user out until they edit files by hand.

- [x] **9. A turn that fails inside the ACP prompt task locks the session for good**
  (`crates/harness-cli/src/acp.rs:320-330`)
  - **What this is:** The prompt handler takes the `Agent` out of its session slot, runs the
    turn, and puts it back with `end_turn`.
  - **Problem:** Two `?` exits skip `end_turn`. One is `send_notification(…)?` inside the event
    loop. The other is `task.await…?`, taken if the agent task panics. After either one, the
    slot's agent stays `None`, so every later prompt or mode change in that session returns
    "a prompt is already running" until the extension restarts. After the send error, the
    agent task also keeps running unseen.
  - **Fix:** On a send failure, cancel the turn and keep draining events instead of returning.
    On a panic, put a fresh agent into the slot (`new_agent`) or remove the session, so the
    client gets a clear error and not a stuck session.
  - **Done when:** A test with an agent whose turn panics (or a fake client that drops
    notifications) shows that the next `session/prompt` on that session is not refused with
    "already running".
  - **If we skip it:** A single bug or a blip on the connection leaves a chat tab dead.

- [x] **10. ACP sessions are never freed, so memory and temp folders only grow**
  (`crates/harness-cli/src/acp.rs:143`)
  - **What this is:** `Server::open` adds every new or loaded session to a `HashMap`, which
    holds its `Agent`: the full history, a `ToolContext`, and spool and temp folders.
  - **Problem:** Nothing ever removes an entry. The VS Code extension's `harness acp` process
    lives as long as the window. Each new chat or session reload adds one more agent, with its
    whole history, plus `/tmp/harness-spool-*` and `harness-tmp-*` folders that are deleted
    only on drop.
  - **Fix:** When a session id is loaded again, replace its old slot (this already happens).
    Also drop the other idle slots when a new session opens, if the client uses only one at a
    time (check what `harness-vscode` does). Or handle the ACP session close request if the
    crate offers one.
  - **Done when:** A test opens two sessions one after the other and shows that the first
    slot is dropped (for example, its spool folder is gone).
  - **If we skip it:** A long VS Code day slowly uses more RAM and fills `/tmp`.

- [x] **11. A failed `/undo` loses the rest of that turn's checkpoints**
  (`crates/harness-core/src/tools/checkpoint.rs:84-100`)
  - **What this is:** `undo` takes the last turn's list of saved copies and restores them one by
    one.
  - **Problem:** The turn is removed from the list before the restore starts. If one restore
    fails, the `?` returns at once. For example, `Bash` deleted the folder of an edited file,
    so `replace_with` can't create its temp file. The files not yet restored stay changed, and
    their copies can no longer be reached with `/undo`.
  - **Fix:** Restore every file and collect the errors. Push the entries that failed back onto
    `turns`. Report them all.
  - **Done when:** A test edits two files in one turn and removes the folder of one of them.
    After `undo`, the other file is restored and the error names the failed file.
  - **If we skip it:** The user loses work in exactly the case undo exists for.

- [x] **12. Skills, commands and agents saved with Windows line endings lose their frontmatter**
  (`crates/harness-core/src/plugin.rs:606-609`)
  - **What this is:** `split_frontmatter` separates the `---` header (name, description, model,
    tools) from the body.
  - **Problem:** It matches only `---\n`. With CRLF (`---\r\n`) line endings, or a closing `---`
    at the very end of the file with no newline after it, no frontmatter is found. Then the
    name falls back to the file name, the description becomes `---`, and `disable-model-invocation`,
    `tools` and `model` are lost. For an agent, losing `tools` means the agent gets every tool
    instead of a few.
  - **Fix:** Accept `\r\n`: strip `\r` at the end of each frontmatter line, and accept a
    closing `\n---` at the end of the file.
  - **Done when:** A test in `crates/harness-core/tests/plugin.rs` parses a CRLF agent file with
    `tools: Read` and gets `Some(["Read"])`.
  - **If we skip it:** Files shared from Windows users act differently, and agents can get more
    tools than they should.

- [x] **13. Checkpoint copies and session logs are never deleted**
  (`crates/harness-core/src/tools/checkpoint.rs`, `crates/harness-core/src/session.rs`)
  - **What this is:** Each turn copies every file it changes into
    `~/.local/share/harness/sessions/<id>/checkpoints/`. Logs are append-only.
  - **Problem:** Nothing ever deletes them. Editing a 5 MB file in 100 turns keeps 500 MB.
    This is per user, not per session, so it adds up over months.
  - **Fix:** Keep checkpoints only for the last N turns (undo goes back step by step, so old
    ones are almost never used). Delete the copy when its turn falls out of the window. Mark
    the log retention as a `shortcut:` with its limit if it stays for now.
  - **Done when:** A test runs N+1 turns with edits and shows that the oldest copy is gone.
  - **If we skip it:** Disk use grows slowly and without notice.

## Nice to have

- [x] **14. Unsaved buffers are not used when the workspace path has a symlink in it**
  (`crates/harness-vscode/src/protocol.rs:596-602`)
  - **What this is:** The extension answers `fs/read_text_file` only for paths inside its `cwd`.
  - **Problem:** The harness canonicalises the workspace (it resolves symlinks), but the
    extension compares against VS Code's path, which may not be resolved. With a symlinked
    folder (for example `~/work`, a link to `/mnt/4TB/Work`), every request is refused, and
    `Read` silently falls back to the disk.
  - **Fix:** Send the session's `cwd` as the canonical path the harness uses, or compare against
    it in `inside`.
  - **Done when:** A native test of `inside` (or of the session setup) covers a symlinked root.
  - **If we skip it:** Unsaved edits are quietly ignored for some users.

---

**Verdict:** Fix 1 first (it escapes the sandbox). Then fix 2 and 3, because together they make
resume unusable.

**Lean:** about -30 lines possible (item 8 removes the hand-written temp-file code; item 7
removes unused `Outcome` fields). No dependency can be dropped.

**Not checked:** `xtask/` (eval runner) and `scripts/prove-evals` were only skimmed. Most of
`harness-vscode/src/lib.rs` and `harness-vscode-ui/src/lib.rs` (DOM and process wiring) was
not read, except the markdown escaping, the `read_file` guard and the process spawn. In
`context/` (budget, compaction, prefix), only the turn-boundary logic was checked. `provider/admin.rs` (model load/unload) was not read; there is no Anthropic provider code yet. `./scripts/verify` was not run
for this audit, and nothing was changed in the code.
