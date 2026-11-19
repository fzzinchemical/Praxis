## 6. Tools

**Names and parameters match Claude Code exactly** because plugin hooks match on them (`"matcher": "Write|Edit"`) and read `tool_input.file_path`. Changing them breaks plugin compatibility.

| Tool | Parameters | Default permission |
|---|---|---|
| `Read` | `file_path`, `offset?`, `limit?` | allow in workspace |
| `Write` | `file_path`, `content` | ask |
| `Edit` | `file_path`, `old_string`, `new_string`, `replace_all?` | ask |
| `Bash` | `command`, `description?`, `timeout?` (ms) | ask, sandboxed |
| `Grep` | `pattern`, `path?`, `glob?`, `output_mode?`, `-i?`, `multiline?` | allow in workspace |
| `Glob` | `pattern`, `path?` | allow in workspace |

Later additions: `Skill` (T6.2), `ToolSearch` and `CallTool` (T4.4).

```rust
pub struct ToolOutput { pub content: String, pub is_error: bool, pub structured: serde_json::Value }
```

`structured` is what `PostToolUse` hooks receive as `tool_response`: `Write` gives `filePath`, `type`; `Bash` gives `stdout`, `stderr`, `interrupted`, `isImage`.

Behaviour:

- **Paths** are absolute in `tool_input`. Resolve relative paths against the workspace before hooks see them.
- **`Edit`** matches exactly first. If nothing matches, it retries on whole lines after folding CRLF, Unicode quotes, dashes and NBSP, then again ignoring leading and trailing whitespace; a tier applies only where it is unique (or with `replace_all`), and only the matched text is replaced. It fails if `old_string` is still missing or, without `replace_all`, not unique. Say which, and show the closest block with line numbers, since weak models often get whitespace wrong. Require the file to have been read earlier in the session, and refuse if it changed since that read, so a developer's concurrent edit is never overwritten.
- **`Write` and `Edit` results** stay short (path and lines changed) and never repeat file content to the model. With an editor that reports diagnostics (§11), a result also ends with that file's error lines, at most 20, labelled as editor text and never as instructions. Hooks still receive the full `structured` value.
- **`Write` and `Edit`** write through a temp file and atomic rename, and preserve file mode. Checkpoint first (§8).
- **`Grep`** defaults `output_mode` to `content` (matching lines with line numbers), unlike Claude Code's `files_with_matches`: with file names only, small models grep again to find where to read, and get stopped as a loop. It stops collecting when the result reaches the agent's result budget (§7), and says so with a note.
- **`Read`** returns line-numbered text, refuses binary files, and applies a default line limit of 2000 lines. It also stops at a line boundary inside the result budget (§7) and notes the next `offset`, so a Read is never cut or spooled. A single line longer than the budget is cut. Never filter or minify its content, because `Edit` needs an exact match.
- **`Bash`** runs through `praxis sandbox-exec` (§9), kills the whole process group on timeout, on cancel and when the shell exits (a leftover background job would hold the output pipes open), and has a default timeout of 120 s. Without `unsafe`, the kill goes through the shell's `kill` builtin.
- **Tool errors** are returned to the model as content. They never end the turn.

