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

## Follow-ups (gaps left after the fixes)

- [x] **F1. Item 9's notification-failure path has no test**
  (`crates/harness-cli/src/acp.rs`, prompt handler)
  - **Problem:** When `send_notification` fails, the turn is now cancelled and the loop keeps
    draining events. No test reaches this path, because closing the connection mid-turn has no
    seam in the test client yet.
  - **Done when:** A test drops the client connection during a turn and shows that the agent
    goes back to its slot.
  - **Closed without a test (2026-11-13):** The path can't be reached with
    `agent-client-protocol` 5.0.0. `send_notification` fails only once the outgoing queue has
    closed, and that happens only when the connection shuts down. The prompt loop runs as a
    `cx.spawn` task inside the connection driver, and the driver drops it in the same step, so no
    running task ever sees the error. A probe test confirmed this: with the client's reads broken
    mid-turn, the connection ended with `client gone`, the loop was dropped, and the error branch
    never ran. `serve_acp` then returns and the process exits, so no stuck slot outlives the
    connection. The `cancel` branch stays as a guard in case a later crate version keeps tasks alive.

- [x] **F2. Item 9's panic test bypasses the protocol**
  (`crates/harness-cli/src/acp.rs`, `a_crashed_turn_does_not_leave_the_next_prompt_refused_as_running`)
  - **Problem:** The test calls `settle_turn` directly with a `JoinError`. It does not prove that
    the prompt handler sends the client an error, because a turn can't be made to panic through
    `session/prompt`.
  - **Done when:** A test makes a turn panic through `session/prompt` and gets an error response.

- [x] **F3. Tests for items 3, 9, 10 and 11 were never seen failing on the old code**
  - **Problem:** Each fix and its test went in together, or the first test run failed to compile.
    So nothing shows that these tests catch the original bug.
  - **Done when:** Each test fails with its fix reverted (check locally, no commit needed), and
    the result is noted here.
  - **Result (2026-11-13):** All four fail with their fix reverted: item 3 (`left: ["c1"]`,
    `right: ["c1", "c2", "c3"]`), item 9 ("a prompt is already running"), item 10 ("a closed
    session must refuse prompts"), item 11 (the error doesn't name `gone/b.txt`).

- [x] **F4. Checkpoint copies and session logs from earlier runs are never deleted**
  (`crates/harness-core/src/tools/checkpoint.rs`, `crates/harness-core/src/session.rs`)
  - **Problem:** Item 13 keeps copies only for the last 20 turns, but only within one process. A
    resumed session starts with an empty turn list, so copies from earlier runs stay on disk. Logs
    and `sessions/<id>/` folders are never deleted (marked with a `shortcut:` comment).
  - **Fix:** On `checkpoint_into`, delete copies numbered below `highest - CHECKPOINT_TURNS`, or
    expire whole session folders by age.
  - **Done when:** A test resumes a session whose folder holds more than 20 old copies, and the
    oldest ones are gone after the next turn.

- [x] **F5. A failed `/undo` doesn't say which files it did restore**
  (`crates/harness-core/src/tools/checkpoint.rs` `undo`)
  - **Problem:** When some restores fail, `undo` returns only the error, which names the failed
    files. The user isn't told which files were restored.
  - **Done when:** The error (or the CLI and ACP message) lists the restored files too.

- [x] **F6. A hooks test may hang under heavy load**
  (`crates/harness-core/src/hooks.rs` tests)
  - **Problem:** During the parallel fix work (load average about 100), one `cargo test -p
    harness-core` run seemed to hang in a hooks test, and passed on a rerun. This was not
    investigated. It may be a timing-sensitive timeout test.
  - **Done when:** The test is found and made deterministic, or a stress run (for example 20
    runs under `stress`) shows that it doesn't hang.

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

---

# Round 2 (2026-11-13)

Round 2 reviewed the 14 fix commits (`facccf7`..`ff46a8f`) and the parts that round 1 skipped:
subagent delegation, `provider/admin.rs`, memory loading in `context/prefix.rs`, the context
budget, the VS Code panel and session wiring, and the `xtask` eval sandbox. Same rules as above:
one item per branch, write the test first, tick the box here.

## Must fix

- [x] **15. A subagent ignores plan mode and the session's permission choices**
  (`crates/harness-core/src/agent/mod.rs:349`, `:247`, `:501`)
  - **What this is:** The `Agent` tool runs a subagent. `delegate` builds it from
    `self.config.clone()`, and `Agent::new` builds its permissions from `config.policy`.
  - **Problem:** `set_mode` (plan mode, and the VS Code permission picker) changes only
    `self.permissions`, not `config.policy`. So the subagent starts with the mode from the
    config file. The session's "always" grants and "reject for this session" choices aren't
    passed on either. `Agent` counts as a read-only tool, so it is allowed in plan mode.
    Example: the user config says `auto-edit`, and the user switches the session to plan mode
    (read-only). The model calls `Agent`, and the subagent writes files without asking. In
    `ask` mode, the user gets write prompts during "plan mode", which should never happen.
  - **Fix:** Give the child a copy of the parent's current permissions
    (`child.permissions = self.permissions.clone()`; add `Clone` to `Permissions`), instead of
    building new ones from `config.policy`.
  - **Done when:** A test in `crates/harness-core/tests/plan.rs` builds an agent with
    `Mode::AutoEdit` and an agent definition, calls `set_mode(Mode::ReadOnly)`, and has the fake
    model call `Agent`, whose child then calls `Write`. The file must not exist afterwards.
  - **If we skip it:** Plan mode is not read-only: one tool call gets around it.

- [x] **16. Two panels on the same session write one log and fight over checkpoints**
  (`crates/harness-vscode/src/lib.rs:296-298`, `crates/harness-core/src/session.rs` `create`/`resume`)
  - **What this is:** Clicking a session in the sidebar runs `harness.openSession`. That always
    opens a new panel, and each panel starts its own `harness acp` process, which resumes the
    session log.
  - **Problem:** Click the same session twice (or keep it open in VS Code and also run
    `harness --resume` in a terminal), and two processes append to one `.jsonl` file. Their
    messages mix, so the next resume rebuilds a history that neither process had. Both also
    read the highest checkpoint number at start and then create the same next number. One of
    them fails with "File exists" on its next `Write` or `Edit`. This is item 2 again, in a new
    way.
  - **Fix:** Take an exclusive lock on the log file in `SessionLog::create` and `resume`, using
    std's `File::try_lock` (stable since Rust 1.89, so no new dependency). If the lock fails,
    report "session `<id>` is open in another window". The lock is released when the log is
    dropped. Optionally, the extension can also show the panel that already exists.
  - **Done when:** A test in `crates/harness-core/tests/session.rs` resumes one id twice. The
    second call returns an error while the first log is still alive, and succeeds after it is
    dropped.
  - **If we skip it:** The session history gets mixed up for good, and file edits fail at
    random.

## Should fix

- [x] **17. Absolute paths (item 4) let a symlinked path get around its deny rule**
  (`crates/harness-core/src/agent/mod.rs` `absolute_paths`,
  `crates/harness-core/src/permissions/mod.rs` `subjects`)
  - **What this is:** `subjects` matches deny rules against two spellings of a path: the one
    the model wrote and the resolved one. Its comment says "so neither `..` nor a symlink can
    step around a deny rule".
  - **Problem:** `check_call` now replaces the model's path with the result of
    `confine::resolve`, which follows symlinks, before the permission check. So both spellings
    are the resolved target. Example: the repo has `secrets -> vendor/keys`, and the deny rule
    is `Read(secrets/**)`. `Read secrets/id` becomes `<ws>/vendor/keys/id`, no deny rule
    matches, and the file is read. Before item 4, this was refused. The unit tests in
    `permissions` call `decide` directly, so they don't see this.
  - **Fix:** In `absolute_paths`, make the path absolute without following symlinks
    (`workspace.join(raw)` for a relative `raw`). Then `subjects` still sees the symlink name,
    and still resolves the target itself.
  - **Done when:** A test in `tests/security.rs` (or `tests/agent.rs`) runs a full agent turn
    with a symlinked folder and a deny rule on the link name, and shows that `Read` through the
    link is denied.
  - **If we skip it:** Deny rules on symlinked folders silently stop working.

- [x] **18. `Write` still overwrites files that have unsaved changes in VS Code**
  (`crates/harness-core/src/tools/files.rs` `write`)
  - **What this is:** Item 5 made `Edit` refuse a file whose editor buffer differs from the
    disk.
  - **Problem:** `Write` has no such check. It replaces the file on disk under a dirty buffer.
    Then VS Code reports a conflict, and either the user's unsaved edits or the agent's write
    is lost, depending on which button the user presses.
  - **Fix:** Use the same `editor_text` comparison in `write` when the file already exists.
    Move it into a small shared helper used by `edit` and `write`.
  - **Done when:** The ACP test for item 5 gets a twin for `Write`, which expects the
    "unsaved changes" message and an unchanged file.
  - **If we skip it:** The data loss from item 5 stays possible through the other write tool.

## Nice to have

- [x] **19. Lost trust-store updates can let a changed MCP tool through without a prompt**
  (`crates/harness-core/src/trust.rs` `set`)
  - **What this is:** Item 8 made each write atomic. But `set` still reads, changes and writes
    the whole file with no lock.
  - **Problem:** Suppose two processes start at the same time and both pin MCP tools. The
    second write drops the first one's pins. On the next start, those tools count as never
    seen, and they are pinned silently in whatever form they have then. If the server changed
    a tool in between, nobody is asked, and catching exactly that is the point of pinning.
    This needs a race and a changed tool, so it is rare.
  - **Fix:** Hold `File::lock` on a `trust.json.lock` file (std, no dependency) around the
    read-modify-write in `set`.
  - **Done when:** The `concurrent_sets_leave_the_store_valid_json` test also checks that all
    16×20 keys are present.
  - **If we skip it:** Rarely, a changed MCP tool gets in without approval.

- [x] **20. Checkpoint copies of undone turns, and of ended sessions, are never deleted**
  (`crates/harness-core/src/tools/checkpoint.rs` `undo`, `begin_turn`)
  - **What this is:** Item 13 deletes copies when a turn drops out of the 20-turn window.
  - **Problem:** `undo` pops a turn but leaves its copy files on disk. Those copies are no
    longer in `turns`, so nothing will ever delete them. The last 20 turns of every session are
    also kept forever after the session ends. Session logs have a `shortcut:` note about the
    same kind of growth; checkpoints have none.
  - **Fix:** Delete each copy after it has been restored in `undo`. Mark the leftovers from
    ended sessions with a `shortcut:` next to the existing one for logs, or delete
    `checkpoints/` when a session that ran no turns for a while is listed.
  - **Done when:** The undo test checks that the checkpoint folder is empty after undoing every
    turn.
  - **If we skip it:** Disk use grows slowly.

---

**Round 2 verdict:** Fix 15 first: plan mode has a hole that one tool call opens. Then fix 16,
which also stops item 2 from coming back.

**Checked and fine:** the `.git` denial (1), the hook timeout (6), CRLF frontmatter (12), the
orphaned tool calls (3), atomic store writes (8), the ACP turn settlement and close (9, 10),
undo with partial failures (11). Also fine: memory and rule imports stay confined (symlinks and
`~/` imports outside the workspace are refused), the branch name read from `HEAD` is filtered,
and the eval runner runs the agent under bwrap plus Landlock.

**Not checked:** `harness-vscode-ui/src/lib.rs` beyond the markdown path, the compaction
summary request, `scripts/prove-evals`. `./scripts/verify` was not run, and none of the round 2
findings were reproduced with a test. They come from reading the code.

---

# Round 3 (2026-11-13)

Round 3 reviewed the round 2 fix commits (`5eae6a9`..`48f646c`) and parts not read before: the
CLI startup flow (`harness-cli/src/main.rs` `run`, `ready_model`, `plugin_command`), the VS Code
protocol and review wiring (`harness-vscode/src/protocol.rs`, `lib.rs` `react`, `open_review`,
`editor_text`, `refresh_sessions`), and the VS Code setting scopes in `package.json`. Same
rules: one item per branch, write the test first, tick the box here.

## Should fix

- [x] **21. Every finished turn starts a whole `harness acp`, MCP servers included, just to list sessions**
  (`crates/harness-vscode/src/lib.rs:711-718`, `:1012`; `crates/harness-cli/src/main.rs` `run`)
  - **What this is:** The sidebar lists sessions by starting a separate `harness acp` and sending
    `session/list`. `react` calls `refresh_sessions` after every `done` message, so this happens
    after every turn.
  - **Problem:** `harness acp` runs the full startup before it answers. It loads the config,
    calls `ready_model`, which loads a model when `agent.auto_load = true`, and connects every
    MCP server: one process each, with up to 10 s `CONNECT_TIMEOUT`, and pinning. So with three
    stdio MCP servers, each turn starts three more server processes that are killed again at
    once. The sidebar also waits for the slowest server. Listing sessions needs none of this:
    it only reads `~/.local/share/harness/sessions`.
  - **Fix:** Add `harness sessions --json`, which prints `session::list(dir, cwd)` like
    `plugin list --json` does, and have `refresh_sessions` run it the way the Plugins view runs
    its CLI. Then delete `protocol::list_request`, `listing_on_line` and the listing child
    wiring. This removes code.
  - **Done when:** A CLI test in `crates/harness-cli/tests/cli.rs` runs `harness sessions
    --json` with no LM Studio reachable and gets the sessions of the temp workspace. A native
    test in `harness-vscode` maps that JSON to `SessionItem`s.
  - **If we skip it:** Turns end more slowly, MCP servers are started over and over (with their
    side effects), and a sidebar refresh can load a model.

- [x] **22. Clean files whose text VS Code normalizes are refused as "unsaved changes"**
  (`crates/harness-vscode/src/lib.rs:790-804` `editor_text`,
  `crates/harness-core/src/tools/files.rs` `refuse_unsaved_buffer`)
  - **What this is:** For an open document, the extension answers `fs/read_text_file` with
    `document.getText()`. Items 5 and 18 refuse `Edit`/`Write` when that text differs from the
    bytes on disk.
  - **Problem:** VS Code's text differs from the disk even when nothing is unsaved. It drops a
    UTF-8 BOM, and it changes mixed line endings into one style when it loads the file. With
    such a file open in a tab, every `Edit` and `Write` says "has unsaved changes in the
    editor; ask the user to save it". Saving doesn't help, because the document isn't dirty,
    so VS Code writes nothing. The agent can't change that file while the tab is open.
  - **Fix:** In `editor_text`, return `getText()` only when `document.isDirty`; otherwise read
    the disk as the fallback already does. Then "differs from disk" really means "unsaved".
  - **Done when:** A native test of the extracted choice (dirty → buffer, clean → disk) in
    `harness-vscode`. Optionally an ACP test where the client serves a BOM-less copy of a clean
    BOM file and `Edit` succeeds.
  - **If we skip it:** The agent is stuck on some files for reasons the user can't see or fix.

- [x] **23. One broken plugin empties the whole Plugins view, so it can't be removed there**
  (`crates/harness-cli/src/main.rs:122-136`)
  - **What this is:** `harness plugin list --json` feeds the VS Code Plugins tree. For each
    installed plugin it calls `store.get` and `plugin.runs`, each with `?`.
  - **Problem:** If one plugin fails to load (a broken `plugin.json`, a hook file with bad
    JSON, a component path that no longer exists after an update), the whole command fails.
    The tree then shows only an error, and the Remove and Disable actions, which work per item,
    can't be reached for any plugin, the broken one included. The plain `plugin list` doesn't
    load plugins, so it still works.
  - **Fix:** On a load error, still print the entry, with `"error": "<message>"` and an empty
    `can_run`. Show it in the tree as an item that has the Remove action.
  - **Done when:** A test in `crates/harness-cli/tests/plugin_cli.rs` installs two plugins,
    breaks one's manifest, and gets both from `plugin list --json`, the broken one with an
    `error`.
  - **If we skip it:** One bad plugin update locks the user out of plugin management in VS Code.

## Nice to have

- [x] **24. `Write` now reads the whole old file every time, with no size cap, even without an editor**
  (`crates/harness-core/src/tools/files.rs:184`)
  - **What this is:** Item 18 added `fs::read(&path)` before every `Write` over an existing
    file, to compare it with the editor buffer.
  - **Problem:** It runs in the CLI too, where there is no editor to compare against. It uses
    `fs::read`, not `read_bytes`, so the 64 MB cap and the regular-file check are skipped.
    Overwriting a large generated file reads all of it into memory for nothing.
  - **Fix:** Read the disk only when `editor_text` returns `Some`: ask the editor first, then
    compare. Use `read_bytes` for the read.
  - **Done when:** A unit test, or the reasoning in the review, shows that a `Write` with no
    editor reader makes no read call. A test with a fake reader still refuses a dirty buffer.
  - **If we skip it:** Wasted I/O, and a large file could use a lot of memory.

---

**Round 3 verdict:** No must-fix this round. Fix 21 first: it costs time on every turn and
starts processes nobody asked for, and its fix deletes code. Then 22, which is a false refusal
that items 5 and 18 brought in.

**Lean:** item 21 deletes the session-listing protocol path (`list_request`, `listing_on_line`,
the listing child): about -60 lines in `harness-vscode`, plus about +20 for the CLI command.

**Checked and fine:** subagents now get the parent's live permissions (15). Log locking,
together with dropping the old slot on reload (16), works. Path folding (17) keeps symlink names
visible to deny rules, and `..` that leaves the workspace still ends in the tool's refusal.
`trust.json` locking (19) holds the lock across the read-modify-write. Copies are deleted after
undo (20). The review diffs are untitled documents, so the files on disk are never touched before
approval. `harness.binaryPath` and `harness.lmstudio.baseUrl` are machine-scoped. Model output
reaches the panel only through escaped markdown. Without a terminal, project config widening
and changed MCP tools both stay inactive.

**Not checked:** `harness-vscode-ui/src/lib.rs` DOM code, the compaction summary request,
`scripts/prove-evals`, `harness-vscode/src/plugins.rs`. `./scripts/verify` was not run, and none
of the round 3 findings were reproduced with a test. The BOM and EOL behaviour in item 22 is
VS Code's documented text model behaviour; it was not tried in a running editor.
