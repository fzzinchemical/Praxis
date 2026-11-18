//! ACP client framing and the panel message mapping, kept free of `JsValue` so it tests natively.
//!
//! Lines are framed by hand with `serde_json` rather than the `agent-client-protocol` crate (SPEC §11).

use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Component, Path};

/// A message from the webview. The webview is untrusted, so anything else is dropped.
#[derive(Debug, PartialEq)]
pub enum FromPanel {
    Ready,
    /// The text, the active file the chip kept (if any), files added with "Add file", and whether
    /// to attach the active file's problems.
    Prompt {
        text: String,
        file: Option<String>,
        files: Vec<String>,
        problems: bool,
    },
    /// The stop button.
    Cancel,
    /// The "Add file" button: ask the user for files to attach.
    AddFile,
    /// Raw URIs from a drop onto the panel (`text/uri-list` lines); see `dropped_paths`.
    Dropped(Vec<String>),
    /// A permission request id (a JSON-RPC number or string) and the chosen option, `None` meaning dismissed.
    Permission(Value, Option<String>),
    /// The model picker's choice, as the agent's model key.
    Model(String),
    /// The permission mode picker's choice: `read-only`, `ask` or `auto-edit`.
    PermissionMode(String),
    /// A changed file the panel asked to open, with the `Write` or `Edit` call that changed it when
    /// the panel knows it. The path is checked against the workspace before use.
    OpenChange {
        path: String,
        call: Option<String>,
    },
    /// The tool call id of a running subagent the panel asked to stop.
    StopSubagent(String),
    /// The user turn the panel asked to rewind to ("Rewind to here"), as the agent numbers it.
    Rewind(u64),
    /// The file the review asked to revert to its state before the change set.
    RevertFile(String),
    /// The gear next to the model picker asked for the selected model's profile.
    ProfileShow,
    /// One profile field the popover saved; `field` is one of `profile::FIELDS`.
    ProfileSet {
        field: String,
        value: String,
    },
}

impl FromPanel {
    pub fn parse(message: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(message).ok()?;
        match value["type"].as_str()? {
            "ready" => Some(Self::Ready),
            "prompt" => Some(Self::Prompt {
                text: value["text"].as_str()?.to_owned(),
                file: value["file"].as_str().map(str::to_owned),
                files: string_array(value["files"].as_array().map_or(&[], Vec::as_slice))?,
                problems: value["problems"].as_bool().unwrap_or(false),
            }),
            "cancel" => Some(Self::Cancel),
            "add_file" => Some(Self::AddFile),
            "dropped" => {
                let uris = string_array(value["uris"].as_array()?)?;
                Some(Self::Dropped(uris))
            }
            "model" => Some(Self::Model(value["key"].as_str()?.to_owned())),
            "mode" => Some(Self::PermissionMode(value["key"].as_str()?.to_owned())),
            "open_change" => Some(Self::OpenChange {
                path: value["path"].as_str()?.to_owned(),
                call: value["call"].as_str().map(str::to_owned),
            }),
            "stop_subagent" => Some(Self::StopSubagent(value["id"].as_str()?.to_owned())),
            "rewind" => Some(Self::Rewind(value["turn"].as_u64()?)),
            "revert_file" => Some(Self::RevertFile(value["path"].as_str()?.to_owned())),
            "profile_show" => Some(Self::ProfileShow),
            "profile_set" => {
                // The value goes to the CLI's argv and then into the config file, so a newline could write extra lines.
                let field = value["field"]
                    .as_str()
                    .filter(|f| crate::profile::FIELDS.contains(f))?;
                let text = value["value"]
                    .as_str()
                    .filter(|v| !v.contains(['\n', '\r']))?;
                Some(Self::ProfileSet {
                    field: field.to_owned(),
                    value: text.to_owned(),
                })
            }
            "permission" => Some(Self::Permission(
                request_id(&value["id"])?.clone(),
                value["option"].as_str().map(str::to_owned),
            )),
            _ => None,
        }
    }
}

/// Editor context attached to one prompt (SPEC §11). Paths are absolute.
#[derive(Debug, Default, PartialEq)]
pub struct Context {
    /// Files attached as `resource_link` blocks.
    pub files: Vec<String>,
    /// The active selection, attached as an embedded `resource`.
    pub selection: Option<Selection>,
    /// The problems of the active file, attached as a text block.
    pub problems: Option<Problems>,
}

#[derive(Debug, PartialEq)]
pub struct Selection {
    pub path: String,
    pub text: String,
    /// 1-based, inclusive.
    pub start_line: u64,
    pub end_line: u64,
}

#[derive(Debug, PartialEq)]
pub struct Problems {
    pub path: String,
    pub items: Vec<Problem>,
}

#[derive(Debug, PartialEq)]
pub struct Problem {
    /// 1-based.
    pub line: u64,
    /// `error`, `warning`, `info` or `hint`.
    pub severity: &'static str,
    pub message: String,
}

/// The `severity` of a VS Code diagnostic, where 0 is an error.
pub fn severity(code: u64) -> &'static str {
    ["error", "warning", "info", "hint"]
        .get(code as usize)
        .copied()
        .unwrap_or("hint")
}

/// The ACP content blocks of a prompt. Anything outside the workspace is dropped, since the
/// webview and the file picker are both untrusted.
pub fn prompt_blocks(text: &str, context: &Context, root: &str) -> Vec<Value> {
    let mut blocks = vec![json!({"type": "text", "text": text})];
    for path in context.files.iter().filter(|path| inside(root, path)) {
        blocks.push(
            json!({"type": "resource_link", "uri": format!("file://{path}"),
            "name": Path::new(path).file_name().and_then(|name| name.to_str()).unwrap_or(path)}),
        );
    }
    if let Some(selection) = context.selection.as_ref().filter(|s| inside(root, &s.path)) {
        blocks.push(json!({"type": "resource", "resource": {
            "uri": format!("file://{}#L{}-{}", selection.path, selection.start_line, selection.end_line),
            "text": selection.text}}));
    }
    if let Some(problems) = context
        .problems
        .as_ref()
        .filter(|p| inside(root, &p.path) && !p.items.is_empty())
    {
        let lines: Vec<String> = problems
            .items
            .iter()
            .map(|item| format!("- line {} {}: {}", item.line, item.severity, item.message))
            .collect();
        let text = format!("Problems in {}:\n{}", problems.path, lines.join("\n"));
        blocks.push(json!({"type": "text", "text": text}));
    }
    blocks
}

/// A line of status text for the panel.
pub fn notice(text: &str) -> Value {
    json!({"type": "notice", "text": text})
}

/// The file the active editor shows, or `None` for no editor.
pub fn active_file(path: Option<&str>) -> Value {
    json!({"type": "active", "path": path})
}

/// Files the user picked with "Add file".
pub fn picked_files(files: &[String]) -> Value {
    json!({"type": "picked", "files": files})
}

/// Local file paths from dropped URIs. Other schemes, comments and bad escapes are skipped.
pub fn dropped_paths(uris: &[String]) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for line in uris.iter().map(|uri| uri.trim()) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix("file://") else {
            continue;
        };
        // `file:///path` and `file://localhost/path`; any other host is not this machine.
        let path = rest.strip_prefix("localhost").unwrap_or(rest);
        if !path.starts_with('/') {
            continue;
        }
        if let Some(path) = percent_decode(path).filter(|path| !paths.contains(path)) {
            paths.push(path);
        }
    }
    paths
}

/// Decodes `%XX` escapes. `None` for a bad escape or bytes that are not UTF-8.
fn percent_decode(text: &str) -> Option<String> {
    let mut out = Vec::with_capacity(text.len());
    let mut bytes = text.bytes();
    while let Some(byte) = bytes.next() {
        if byte != b'%' {
            out.push(byte);
            continue;
        }
        let high = (bytes.next()? as char).to_digit(16)?;
        let low = (bytes.next()? as char).to_digit(16)?;
        out.push((high * 16 + low) as u8);
    }
    String::from_utf8(out).ok()
}

/// Every element must be a string; anything else makes the whole message invalid.
fn string_array(items: &[Value]) -> Option<Vec<String>> {
    items
        .iter()
        .map(|item| item.as_str().map(str::to_owned))
        .collect()
}

/// One call's side of a file change: the content before it starts, then after it succeeds.
#[derive(Debug, PartialEq)]
pub enum Change {
    Before {
        id: String,
        file: String,
        before: String,
    },
    After {
        id: String,
        after: String,
    },
}

/// What one line from `praxis acp` asks the extension to do.
#[derive(Debug, Default, PartialEq)]
pub struct Reaction {
    /// A typed message for the panel (`ui/src/lib.rs` handles each `type`).
    pub show: Option<Value>,
    /// A line for the agent's stdin.
    pub send: Option<String>,
    /// A proposed file change to open in VS Code's diff editor.
    pub review: Option<Review>,
    /// A file the agent asked for, which the host answers with `file_reply` (SPEC §11).
    pub read: Option<FileRead>,
    /// A file whose error problems the agent asked for after an edit, answered by `diagnostics_reply`.
    pub diagnostics: Option<FileRead>,
    /// The model picker's state, sent after `show`.
    pub models: Option<Value>,
    /// The permission mode picker's state, sent after `show`.
    pub modes: Option<Value>,
    /// One `Write` or `Edit` call's file content before and after it, kept by the panel for Open diff.
    pub change: Option<Change>,
    /// The `_praxis/mcp_servers` answer, for the MCP Servers view.
    pub mcp: Option<Value>,
    /// Set when a session was created or loaded, so the MCP Servers view re-lists.
    pub session_ready: bool,
}

/// One saved session as the sessions view lists it.
#[derive(Debug, PartialEq)]
pub struct SessionItem {
    pub id: String,
    pub label: String,
    /// The folder the session was moved to, if any.
    pub folder: Option<String>,
}

/// Maps `praxis sessions --json`, already newest first and limited to one workspace, to sidebar
/// items. An untitled session gets a placeholder label. One malformed row fails the whole listing
/// rather than hiding a session.
pub fn session_items(json: &str) -> Result<Vec<SessionItem>, String> {
    let Value::Array(rows) = serde_json::from_str(json).map_err(|error| error.to_string())? else {
        return Err("sessions did not print an array".to_owned());
    };
    rows.iter()
        .map(|row| {
            let id = row["id"]
                .as_str()
                .ok_or("session row without a string `id`")?;
            let title = row["title"]
                .as_str()
                .ok_or("session row without a string `title`")?;
            let label = if title.is_empty() {
                "Untitled session"
            } else {
                title
            };
            Ok(SessionItem {
                id: id.to_owned(),
                label: label.to_owned(),
                folder: row["folder"].as_str().map(str::to_owned),
            })
        })
        .collect()
}

/// The folders the sessions are in, each once, sorted.
pub fn folder_names(items: &[SessionItem]) -> Vec<String> {
    let names: BTreeSet<&str> = items.iter().filter_map(|i| i.folder.as_deref()).collect();
    names.into_iter().map(str::to_owned).collect()
}

/// Only ids in the session format (`{nanos}-{pid}`, digits only) may reach the CLI. The CLI checks again.
fn is_session_id(id: &str) -> bool {
    id.starts_with(|c: char| c.is_ascii_digit())
        && id.chars().all(|c| c.is_ascii_digit() || c == '-')
}

/// The CLI args that delete one session, or `None` if the id is not in the session id format.
/// `--delete=<id>` keeps an id that starts with `-` from being read as a flag.
pub fn delete_args(id: &str) -> Option<Vec<String>> {
    is_session_id(id).then(|| vec!["sessions".to_owned(), format!("--delete={id}")])
}

/// The CLI args that move one session into `name` (empty removes it from its folder), or `None`
/// for a malformed id. The CLI accepts `NAME` even when it starts with `-`.
pub fn folder_args(id: &str, name: &str) -> Option<Vec<String>> {
    is_session_id(id).then(|| {
        vec![
            "sessions".to_owned(),
            "--folder".to_owned(),
            id.to_owned(),
            name.to_owned(),
        ]
    })
}

/// A proposed change from a permission request's `diff` content. `old_text` is empty for a new file.
#[derive(Debug, PartialEq)]
pub struct Review {
    pub title: String,
    pub old_text: String,
    pub new_text: String,
}

/// The first `diff` item of a tool call, if it has one (SPEC §11, edit review).
fn review_of(tool_call: &Value) -> Option<Review> {
    let diff = tool_call["content"]
        .as_array()?
        .iter()
        .find(|item| item["type"] == "diff")?;
    let path = diff["path"].as_str()?;
    Some(Review {
        title: format!("{}: {path}", tool_call["title"].as_str().unwrap_or("Edit")),
        old_text: diff["oldText"].as_str().unwrap_or_default().to_owned(),
        new_text: diff["newText"].as_str()?.to_owned(),
    })
}

/// An `fs/read_text_file` request whose path has passed the workspace check.
#[derive(Debug, PartialEq)]
pub struct FileRead {
    pub id: Value,
    pub path: String,
}

/// The answer to `_praxis/diagnostics`: only the error-severity problems, so the agent reads
/// what it needs (SPEC §11).
pub fn diagnostics_reply(id: Value, items: &[Problem]) -> String {
    let problems: Vec<Value> = items
        .iter()
        .filter(|item| item.severity == "error")
        .map(|item| json!({"line": item.line, "severity": item.severity, "message": item.message}))
        .collect();
    json!({"jsonrpc": "2.0", "id": id, "result": {"problems": problems}}).to_string()
}

/// The answer to `fs/read_text_file`. `None` means the host could not read the file.
pub fn file_reply(id: Value, text: Option<&str>) -> String {
    match text {
        Some(content) => json!({"jsonrpc": "2.0", "id": id, "result": {"content": content}}),
        None => json!({"jsonrpc": "2.0", "id": id,
            "error": {"code": -32002, "message": "the file cannot be read"}}),
    }
    .to_string()
}

const INITIALIZE: u64 = 0;
const NEW_SESSION: u64 = 1;
const SET_MODEL: u64 = 2;
const LOAD_SESSION: u64 = 4;
const SET_MODE: u64 = 5;
/// Unused by the prompt ids (`next_id` starts above `SET_MODE`), so the rewind answer is never mistaken for one.
const REWIND: u64 = 3;
/// The revert answer. Prompt ids count up from `SET_MODE + 1` and never reach this, so it sits far above them.
const REVERT: u64 = 1 << 40;
/// The MCP list and toggle answers, above the prompt ids for the same reason as `REVERT`.
const MCP_LIST: u64 = 1 << 41;
const MCP_TOGGLE: u64 = (1 << 41) + 1;
const MCP_APPROVE: u64 = (1 << 41) + 2;

/// One ACP connection: `initialize`, then `session/new`, then prompts.
pub struct Client {
    cwd: String,
    session: Option<String>,
    next_id: u64,
    /// Open permission requests: id and the option ids offered.
    pending: Vec<(Value, Vec<String>)>,
    /// The model picker as last shown, re-sent when a switch fails.
    picker: Option<Value>,
    /// The permission mode picker as last shown.
    modes: Option<Value>,
    /// The session a `session/load` asked for, adopted when the agent answers.
    loading: Option<String>,
}

impl Client {
    /// Returns the client and the `initialize` line to send first.
    pub fn start(cwd: &str) -> (Self, String) {
        let client = Self {
            cwd: cwd.to_owned(),
            session: None,
            next_id: SET_MODE + 1,
            pending: Vec::new(),
            picker: None,
            modes: None,
            loading: None,
        };
        (client, initialize())
    }

    /// Like `start`, but after `initialize` the agent loads `session` instead of creating one.
    pub fn resume(cwd: &str, session: &str) -> (Self, String) {
        let (mut client, line) = Self::start(cwd);
        client.loading = Some(session.to_owned());
        (client, line)
    }

    /// The ACP session this connection holds, once the agent has created or loaded it.
    pub fn session_id(&self) -> Option<&str> {
        self.session.as_deref()
    }

    /// The `session/prompt` line, or `None` while no session is open yet.
    pub fn prompt(&mut self, text: &str, context: &Context) -> Option<String> {
        let session = self.session.as_deref()?;
        let blocks = prompt_blocks(text, context, &self.cwd);
        let params = json!({"sessionId": session, "prompt": blocks});
        let line = request(self.next_id, "session/prompt", params);
        self.next_id += 1;
        Some(line)
    }

    /// `session/set_config_option` for the model picker, or `None` while no session is open yet.
    pub fn set_model(&mut self, key: &str) -> Option<String> {
        let session = self.session.as_deref()?;
        let params = json!({"sessionId": session, "configId": "model", "value": key});
        Some(request(SET_MODEL, "session/set_config_option", params))
    }

    /// The model key the picker last showed as current, once the agent has answered.
    pub fn current_model(&self) -> Option<&str> {
        self.picker.as_ref()?["current"].as_str()
    }

    /// `session/set_config_option` for the permission mode picker, or `None` while no session is open yet.
    pub fn set_mode(&mut self, key: &str) -> Option<String> {
        let session = self.session.as_deref()?;
        let params = json!({"sessionId": session, "configId": "mode", "value": key});
        Some(request(SET_MODE, "session/set_config_option", params))
    }

    /// The answer to a pending permission request, or `None` if the panel names one not offered.
    pub fn answer(&mut self, id: &Value, option: Option<&str>) -> Option<String> {
        let index = self.pending.iter().position(|(pending, options)| {
            pending == id && option.is_none_or(|option| options.iter().any(|o| o == option))
        })?;
        self.pending.remove(index);
        let outcome = match option {
            Some(option) => json!({"outcome": "selected", "optionId": option}),
            None => json!({"outcome": "cancelled"}),
        };
        Some(json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": outcome}}).to_string())
    }

    /// `session/cancel`, then `cancelled` for every open permission request, as ACP requires.
    pub fn cancel(&mut self) -> Vec<String> {
        let Some(session) = &self.session else {
            return Vec::new();
        };
        let cancel =
            json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session}});
        let ids: Vec<Value> = self.pending.iter().map(|(id, _)| id.clone()).collect();
        let answers = ids.iter().filter_map(|id| self.answer(id, None));
        std::iter::once(cancel.to_string()).chain(answers).collect()
    }

    /// The `_praxis/rewind` request for a user turn, or `None` while no session is open yet.
    pub fn rewind(&self, turn: u64) -> Option<String> {
        let session = self.session.as_deref()?;
        let params = json!({"sessionId": session, "turn": turn});
        Some(request(REWIND, "_praxis/rewind", params))
    }

    /// The `_praxis/revert_file` request for one changed file, or `None` while no session is open yet.
    pub fn revert_file(&self, path: &str) -> Option<String> {
        let session = self.session.as_deref()?;
        let params = json!({"sessionId": session, "path": path});
        Some(request(REVERT, "_praxis/revert_file", params))
    }

    /// The `_praxis/mcp_servers` request for this session, or `None` while no session is open yet.
    pub fn mcp_servers(&self) -> Option<String> {
        let session = self.session.as_deref()?;
        Some(request(
            MCP_LIST,
            "_praxis/mcp_servers",
            json!({"sessionId": session}),
        ))
    }

    /// The `_praxis/set_mcp_enabled` request, which turns a server on or off for this session only.
    pub fn set_mcp_enabled(&self, name: &str, enabled: bool) -> Option<String> {
        let session = self.session.as_deref()?;
        let params = json!({"sessionId": session, "name": name, "enabled": enabled});
        Some(request(MCP_TOGGLE, "_praxis/set_mcp_enabled", params))
    }

    /// The `_praxis/approve_mcp_server` request. Sent only after the user confirms the server's command.
    pub fn approve_mcp(&self, name: &str) -> Option<String> {
        let session = self.session.as_deref()?;
        let params = json!({"sessionId": session, "name": name});
        Some(request(MCP_APPROVE, "_praxis/approve_mcp_server", params))
    }

    /// The `_praxis/cancel_subagent` notification for a running subagent, or `None` while no session
    /// is open yet. A notification has no id, so the agent sends no answer.
    pub fn stop_subagent(&self, id: &str) -> Option<String> {
        let session = self.session.as_deref()?;
        let params = json!({"sessionId": session, "toolCallId": id});
        Some(
            json!({"jsonrpc": "2.0", "method": "_praxis/cancel_subagent", "params": params})
                .to_string(),
        )
    }

    pub fn on_line(&mut self, line: &str) -> Reaction {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return show(notice(&format!("unreadable agent output: {line}")));
        };
        if let Some(error) = message.get("error") {
            let text = error["data"]
                .as_str()
                .or(error["message"].as_str())
                .unwrap_or("unknown error");
            return match message["id"].as_u64() {
                Some(INITIALIZE | NEW_SESSION) => {
                    show(notice(&format!("Praxis could not start: {text}")))
                }
                Some(LOAD_SESSION) => {
                    self.loading = None;
                    show(notice(&format!("Could not open session: {text}")))
                }
                Some(SET_MODEL) => Reaction {
                    show: Some(notice(&format!("Could not switch model: {text}"))),
                    models: self.picker.clone(),
                    ..Reaction::default()
                },
                Some(REWIND) => show(notice(&format!("Could not rewind: {text}"))),
                Some(REVERT) => show(notice(&format!("Could not revert: {text}"))),
                Some(MCP_LIST) => show(notice(&format!("Could not list MCP servers: {text}"))),
                // Re-list so the view's checkbox shows what the agent really has.
                Some(MCP_TOGGLE) => Reaction {
                    show: Some(notice(&format!("Could not change MCP server: {text}"))),
                    send: self.mcp_servers(),
                    ..Reaction::default()
                },
                Some(MCP_APPROVE) => Reaction {
                    show: Some(notice(&format!("Could not approve MCP server: {text}"))),
                    send: self.mcp_servers(),
                    ..Reaction::default()
                },
                Some(SET_MODE) => Reaction {
                    show: Some(notice(&format!("Could not change permissions: {text}"))),
                    modes: self.modes.clone(),
                    ..Reaction::default()
                },
                _ => show(json!({"type": "done", "reason": format!("error: {text}")})),
            };
        }
        match (message["method"].as_str(), &message["id"]) {
            (Some("session/update"), _) => update(&message["params"]["update"]),
            (Some("_praxis/resources"), _) => show(json!({"type": "resources",
                "ram": message["params"]["ram"], "vram": message["params"]["vram"]})),
            (Some("session/request_permission"), id) => self.permission(id, &message["params"]),
            (Some("fs/read_text_file"), id) => self.read_file(id, &message["params"], false),
            (Some("_praxis/diagnostics"), id) => self.read_file(id, &message["params"], true),
            (Some(_), _) => Reaction::default(),
            (None, id) => self.response(id.as_u64(), &message["result"]),
        }
    }

    fn permission(&mut self, id: &Value, params: &Value) -> Reaction {
        let Some(id) = request_id(id).cloned() else {
            // JSON-RPC ids are numbers or strings; refuse anything else rather than leave the turn hanging.
            let send = json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": {"outcome": "cancelled"}}});
            return Reaction {
                show: Some(notice("permission refused: unsupported request id")),
                send: Some(send.to_string()),
                ..Reaction::default()
            };
        };
        let offered = params["options"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let options: Vec<Value> = offered
            .iter()
            .filter_map(|o| {
                Some(json!({"id": o["optionId"].as_str()?, "name": o["name"].as_str()?}))
            })
            .collect();
        let ids = options
            .iter()
            .filter_map(|o| o["id"].as_str().map(str::to_owned));
        self.pending.push((id.clone(), ids.collect()));
        let title = params["toolCall"]["title"].as_str().unwrap_or("a tool");
        Reaction {
            review: review_of(&params["toolCall"]),
            ..show(json!({"type": "permission", "id": id, "title": title, "options": options}))
        }
    }

    /// Refuses paths outside the workspace. The host answers `fs/read_text_file` with the text,
    /// unsaved first, and `_praxis/diagnostics` with the file's problems.
    fn read_file(&self, id: &Value, params: &Value, diagnostics: bool) -> Reaction {
        let path = params["path"].as_str().unwrap_or_default();
        if !inside(&self.cwd, path) {
            // shortcut: a symlink inside the workspace can point outside and the host follows it;
            // the agent's own Read resolves links first, so this only guards direct requests.
            let refusal = json!({"jsonrpc": "2.0", "id": id,
                "error": {"code": -32602, "message": "path is outside the workspace"}});
            return Reaction {
                send: Some(refusal.to_string()),
                ..Reaction::default()
            };
        }
        let request = Some(FileRead {
            id: id.clone(),
            path: path.to_owned(),
        });
        if diagnostics {
            Reaction {
                diagnostics: request,
                ..Reaction::default()
            }
        } else {
            Reaction {
                read: request,
                ..Reaction::default()
            }
        }
    }

    fn response(&mut self, id: Option<u64>, result: &Value) -> Reaction {
        match id {
            Some(INITIALIZE) => Reaction {
                send: Some(match &self.loading {
                    Some(session) => request(
                        LOAD_SESSION,
                        "session/load",
                        json!({"sessionId": session, "cwd": self.cwd, "mcpServers": []}),
                    ),
                    None => request(
                        NEW_SESSION,
                        "session/new",
                        json!({"cwd": self.cwd, "mcpServers": []}),
                    ),
                }),
                ..Reaction::default()
            },
            Some(NEW_SESSION) => {
                self.session = result["sessionId"].as_str().map(str::to_owned);
                self.picker = picker(result, "model", "models");
                self.modes = picker(result, "mode", "modes");
                Reaction {
                    show: Some(notice("ready")),
                    models: self.picker.clone(),
                    modes: self.modes.clone(),
                    session_ready: true,
                    ..Reaction::default()
                }
            }
            Some(LOAD_SESSION) => {
                self.session = self.loading.take();
                self.picker = picker(result, "model", "models");
                self.modes = picker(result, "mode", "modes");
                Reaction {
                    show: Some(notice("ready")),
                    models: self.picker.clone(),
                    modes: self.modes.clone(),
                    session_ready: true,
                    ..Reaction::default()
                }
            }
            Some(REWIND) => show(json!({"type": "rewound", "turn": result["turn"]})),
            Some(REVERT) => show(json!({"type": "reverted", "path": result["path"]})),
            Some(MCP_LIST) => Reaction {
                mcp: Some(result.clone()),
                ..Reaction::default()
            },
            Some(MCP_TOGGLE | MCP_APPROVE) => Reaction {
                send: self.mcp_servers(),
                ..Reaction::default()
            },
            Some(SET_MODE) => match picker(result, "mode", "modes") {
                Some(modes) => {
                    self.modes = Some(modes.clone());
                    Reaction {
                        modes: Some(modes),
                        ..Reaction::default()
                    }
                }
                None => Reaction::default(),
            },
            Some(SET_MODEL) => match picker(result, "model", "models") {
                Some(models) => {
                    self.picker = Some(models.clone());
                    Reaction {
                        models: Some(models),
                        ..Reaction::default()
                    }
                }
                None => Reaction::default(),
            },
            _ => match result["stopReason"].as_str() {
                Some(reason) => show(json!({"type": "done", "reason": reason})),
                None => Reaction::default(),
            },
        }
    }
}

/// The text shown after "Turn ended:". A turn cut by the request limit names the limit and its settings.
/// `limit` is `None` when only config.toml (or its default) sets it, since the extension can't see that value.
pub fn done_reason(reason: &str, limit: Option<u64>) -> String {
    if reason != "max_turn_requests" {
        return reason.to_owned();
    }
    let limit = limit.map_or_else(|| "the limit".to_owned(), |n| format!("the limit of {n}"));
    format!(
        "stopped at {limit} model requests per turn. To allow more, raise \
         praxis.agent.maxTurnRequests in VS Code settings or agent.max_turn_requests in config.toml."
    )
}

/// The `initialize` line both kinds of `praxis acp` child send first.
pub fn initialize() -> String {
    request(
        INITIALIZE,
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {"fs": {"readTextFile": true},
            "_meta": {"praxis": {"diagnostics": true}}}}),
    )
}

/// The `config` option of a `session/new`, `session/load` or `session/set_config_option` answer,
/// as the panel shows it under the message type `kind`.
fn picker(result: &Value, config: &str, kind: &str) -> Option<Value> {
    let option = result["configOptions"]
        .as_array()?
        .iter()
        .find(|option| option["id"] == config)?;
    let options: Vec<Value> = option["options"]
        .as_array()?
        .iter()
        .filter_map(|o| Some(json!({"value": o["value"].as_str()?, "name": o["name"].as_str()?})))
        .collect();
    Some(json!({"type": kind, "current": option["currentValue"].as_str()?, "options": options}))
}

/// `path` is absolute, under `root` and free of `..`, so it cannot leave the workspace.
pub fn inside(root: &str, path: &str) -> bool {
    let path = Path::new(path);
    // Not `is_absolute`: on wasm32-unknown-unknown it is always false, which refused every path.
    path.has_root()
        && path.starts_with(root)
        && !path.components().any(|part| part == Component::ParentDir)
}

/// The absolute path an open request names, if it is inside the workspace. The agent's tools
/// resolve a relative `file_path` against the workspace, so a relative one is joined to it.
pub fn change_target(root: &str, path: &str) -> Option<String> {
    let full = if Path::new(path).has_root() {
        path.to_owned()
    } else {
        format!("{root}/{path}")
    };
    inside(root, &full).then_some(full)
}

fn update(update: &Value) -> Reaction {
    match update["sessionUpdate"].as_str() {
        Some("agent_message_chunk") => match update["content"]["text"].as_str() {
            Some(text) => show(json!({"type": "text", "text": text})),
            None => Reaction::default(),
        },
        // Sent only when `session/load` replays history; a live prompt is drawn by the panel itself.
        Some("user_message_chunk") => match update["content"]["text"].as_str() {
            Some(text) => show(json!({"type": "user", "text": text})),
            None => Reaction::default(),
        },
        // A subagent's prompt size is its own message, so it never resets the Agent call's status.
        Some("tool_call_update") if update["_meta"]["praxis"]["subagentUsage"].is_object() => {
            let usage = &update["_meta"]["praxis"]["subagentUsage"];
            show(json!({"type": "subagent_usage", "id": update["toolCallId"],
                "promptTokens": usage["promptTokens"], "contextLength": usage["contextLength"]}))
        }
        Some("tool_call" | "tool_call_update") => {
            // Sent once a Write or Edit succeeds, with no status or content: it only records the file.
            if let Some(after) = update["_meta"]["praxis"]["after"].as_str() {
                return Reaction {
                    change: Some(Change::After {
                        id: update["toolCallId"].as_str().unwrap_or_default().to_owned(),
                        after: after.to_owned(),
                    }),
                    ..Reaction::default()
                };
            }
            let texts: Vec<&str> = update["content"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter_map(|item| item["content"]["text"].as_str())
                .collect();
            let output = (!texts.is_empty()).then(|| texts.join("\n"));
            let mut message = json!({"type": "tool", "id": update["toolCallId"], "title": update["title"],
                "status": update["status"], "output": output});
            // Set only when present, so plain tool calls keep their shape.
            let meta = &update["_meta"]["praxis"];
            if let Some(parent) = meta["parentToolCallId"].as_str() {
                message["parent"] = parent.into();
            }
            if let Some(kind) = meta["subagent"].as_str() {
                message["subagent"] = kind.into();
            }
            // Set on a subagent's text update: its output is the new lines to add, not the whole text.
            if meta["append"].as_bool() == Some(true) {
                message["append"] = true.into();
            }
            // Set by the agent on a Write or Edit call only: the file it changes.
            if let Some(file) = meta["filePath"].as_str() {
                message["file"] = file.into();
            }
            if let Some(arg) = key_argument(&update["rawInput"]) {
                message["arg"] = arg.into();
            }
            let mut reaction = show(message);
            if let (Some(file), Some(before)) = (meta["filePath"].as_str(), meta["before"].as_str())
            {
                reaction.change = Some(Change::Before {
                    id: update["toolCallId"].as_str().unwrap_or_default().to_owned(),
                    file: file.to_owned(),
                    before: before.to_owned(),
                });
            }
            reaction
        }
        Some("usage_update") => {
            show(json!({"type": "usage", "used": update["used"], "size": update["size"]}))
        }
        Some("available_commands_update") => {
            let commands: Vec<Value> = update["availableCommands"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .map(|c| {
                    json!({"name": c["name"], "description": c["description"],
                    "source": c["_meta"]["source"]})
                })
                .collect();
            show(json!({"type": "commands", "commands": commands}))
        }
        _ => Reaction::default(),
    }
}

/// The argument a tool card shows beside its title: the file, command or pattern the call works on.
fn key_argument(input: &Value) -> Option<&str> {
    ["file_path", "path", "command", "pattern", "url"]
        .iter()
        .find_map(|key| input[*key].as_str())
}

fn show(message: Value) -> Reaction {
    Reaction {
        show: Some(message),
        ..Reaction::default()
    }
}

/// A JSON-RPC request id: a number or a string. `agent-client-protocol` 5 sends strings.
fn request_id(id: &Value) -> Option<&Value> {
    (id.is_u64() || id.is_string()).then_some(id)
}

fn request(id: u64, method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

/// Splits a stdout stream into whole lines, holding back a trailing partial one.
#[derive(Default)]
pub struct Lines(String);

impl Lines {
    pub fn push(&mut self, chunk: &str) -> Vec<String> {
        self.0.push_str(chunk);
        let Some(end) = self.0.rfind('\n') else {
            return Vec::new();
        };
        let rest = self.0.split_off(end + 1);
        let done = std::mem::replace(&mut self.0, rest);
        done.lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rewind_request_names_the_turn_and_its_answer_routes_to_the_panel() {
        assert_eq!(
            FromPanel::parse(r#"{"type":"rewind","turn":3}"#),
            Some(FromPanel::Rewind(3))
        );
        assert_eq!(FromPanel::parse(r#"{"type":"rewind"}"#), None);

        let mut client = Client::start("/work").0;
        assert_eq!(client.rewind(3), None, "no session yet");
        client.session = Some("s1".into());
        let line: Value = serde_json::from_str(&client.rewind(3).unwrap()).unwrap();
        assert_eq!(line["method"], "_praxis/rewind");
        assert_eq!(line["params"], json!({"sessionId": "s1", "turn": 3}));

        let answer = format!(r#"{{"jsonrpc":"2.0","id":{REWIND},"result":{{"turn":3}}}}"#);
        assert_eq!(
            client.on_line(&answer).show,
            Some(json!({"type": "rewound", "turn": 3}))
        );
        let refused = format!(
            r#"{{"jsonrpc":"2.0","id":{REWIND},"error":{{"code":-32602,"data":"turn 9 can't be rewound to"}}}}"#
        );
        let shown = client.on_line(&refused).show.unwrap();
        assert_eq!(shown["type"], "notice");
        assert_eq!(
            shown["text"],
            "Could not rewind: turn 9 can't be rewound to"
        );
    }

    #[test]
    fn profile_requests_parse_and_reject_unknown_fields_or_multiline_values() {
        assert_eq!(
            FromPanel::parse(r#"{"type":"profile_show"}"#),
            Some(FromPanel::ProfileShow)
        );
        assert_eq!(
            FromPanel::parse(r#"{"type":"profile_set","field":"top_k","value":"-1"}"#),
            Some(FromPanel::ProfileSet {
                field: "top_k".into(),
                value: "-1".into()
            })
        );
        assert_eq!(
            FromPanel::parse(r#"{"type":"profile_set","field":"bogus","value":"1"}"#),
            None
        );
        assert_eq!(
            FromPanel::parse(r#"{"type":"profile_set","field":"top_k","value":"1\n[x]"}"#),
            None
        );
        assert_eq!(
            FromPanel::parse(r#"{"type":"profile_set","field":"top_k"}"#),
            None
        );
    }

    #[test]
    fn a_revert_request_names_the_file_and_its_answer_routes_to_the_review() {
        assert_eq!(
            FromPanel::parse(r#"{"type":"revert_file","path":"src/a.rs"}"#),
            Some(FromPanel::RevertFile("src/a.rs".into()))
        );
        assert_eq!(FromPanel::parse(r#"{"type":"revert_file"}"#), None);

        let mut client = Client::start("/work").0;
        assert_eq!(client.revert_file("src/a.rs"), None, "no session yet");
        client.session = Some("s1".into());
        let line: Value = serde_json::from_str(&client.revert_file("src/a.rs").unwrap()).unwrap();
        assert_eq!(line["method"], "_praxis/revert_file");
        assert_eq!(
            line["params"],
            json!({"sessionId": "s1", "path": "src/a.rs"})
        );
        assert_eq!(line["id"], REVERT);

        let answer = format!(r#"{{"jsonrpc":"2.0","id":{REVERT},"result":{{"path":"src/a.rs"}}}}"#);
        assert_eq!(
            client.on_line(&answer).show,
            Some(json!({"type": "reverted", "path": "src/a.rs"}))
        );
        let refused = format!(
            r#"{{"jsonrpc":"2.0","id":{REVERT},"error":{{"code":-32602,"data":"src/a.rs has no changes"}}}}"#
        );
        let shown = client.on_line(&refused).show.unwrap();
        assert_eq!(shown["type"], "notice");
        assert_eq!(shown["text"], "Could not revert: src/a.rs has no changes");
    }

    fn sent(reaction: &Reaction) -> Value {
        serde_json::from_str(reaction.send.as_deref().unwrap_or("null")).unwrap()
    }

    #[test]
    fn turn_limit_stop_names_the_limit_and_both_settings() {
        let text = done_reason("max_turn_requests", Some(40));
        assert!(text.contains("40"), "{text}");
        assert!(text.contains("praxis.agent.maxTurnRequests"), "{text}");
        assert!(text.contains("agent.max_turn_requests"), "{text}");
        assert!(!done_reason("max_turn_requests", None).contains("50"));
        assert_eq!(done_reason("end_turn", Some(40)), "end_turn");
    }

    #[test]
    fn dropped_uris_keep_local_files_only() {
        let uris = |list: &[&str]| list.iter().map(|uri| (*uri).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            dropped_paths(&uris(&["file:///work/a.rs", "file://localhost/work/b.rs"])),
            ["/work/a.rs", "/work/b.rs"]
        );
        assert!(
            dropped_paths(&uris(&[
                "https://example.com/a.rs",
                "untitled:Untitled-1",
                "file://host/x.rs",
            ]))
            .is_empty()
        );
    }

    #[test]
    fn dropped_uris_decode_percent_escapes() {
        let uris = ["file:///work/my%20file%C3%A9.rs".to_owned()];
        assert_eq!(dropped_paths(&uris), ["/work/my fileé.rs"]);
    }

    #[test]
    fn dropped_uris_skip_comments_blanks_and_bad_escapes() {
        let uris = [
            "# copied from the file manager".to_owned(),
            String::new(),
            "file:///work/bad%zz.rs".to_owned(),
            "file:///work/cut%2".to_owned(),
            "file:///work/%FF.rs".to_owned(),
            " file:///work/ok.rs\r".to_owned(),
        ];
        assert_eq!(dropped_paths(&uris), ["/work/ok.rs"]);
    }

    #[test]
    fn dropped_uris_are_deduplicated() {
        let uris = ["file:///a.rs".to_owned(), " file:///a.rs ".to_owned()];
        assert_eq!(dropped_paths(&uris), ["/a.rs"]);
    }

    #[test]
    fn a_subagents_written_file_reaches_the_panel_for_its_change_list() {
        let (mut client, _) = Client::start("/work");
        let write = json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "Write a.rs",
            "status": "pending", "_meta": {"praxis": {"parentToolCallId": "c0", "filePath": "/work/a.rs"}}});
        let line = json!({"jsonrpc": "2.0", "method": "session/update",
            "params": {"sessionId": "s1", "update": write}});
        let shown = client.on_line(&line.to_string()).show.expect("shown");
        assert_eq!(shown["file"], "/work/a.rs");
        assert_eq!(shown["parent"], "c0");
    }

    #[test]
    fn a_write_carries_the_file_as_it_was_before_the_call() {
        let (mut client, _) = Client::start("/work");
        let edit = json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "Edit a.rs",
            "status": "pending", "_meta": {"praxis": {"filePath": "/work/a.rs", "before": "old\n"}}});
        let line = json!({"jsonrpc": "2.0", "method": "session/update",
            "params": {"sessionId": "s1", "update": edit}});
        let reaction = client.on_line(&line.to_string());
        assert_eq!(
            reaction.change,
            Some(Change::Before {
                id: "c1".to_owned(),
                file: "/work/a.rs".to_owned(),
                before: "old\n".to_owned(),
            })
        );
        assert!(
            reaction.show.expect("shown")["before"].is_null(),
            "the webview never gets the content"
        );
    }

    #[test]
    fn a_finished_write_carries_the_file_as_it_is_after_the_call() {
        let (mut client, _) = Client::start("/work");
        let after = json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1",
            "_meta": {"praxis": {"after": "new\n"}}});
        let line = json!({"jsonrpc": "2.0", "method": "session/update",
            "params": {"sessionId": "s1", "update": after}});
        let reaction = client.on_line(&line.to_string());
        assert_eq!(
            reaction.change,
            Some(Change::After {
                id: "c1".to_owned(),
                after: "new\n".to_owned(),
            })
        );
        assert_eq!(
            reaction.show, None,
            "the change must not touch the call's status"
        );
    }

    #[test]
    fn only_paths_inside_the_workspace_can_be_opened() {
        assert!(inside("/work", "/work/src/a.rs"));
        assert!(!inside("/work", "/work2/a.rs"));
        assert!(!inside("/work", "/work/../etc/passwd"));
        assert!(!inside("/work", "src/a.rs"));
    }

    #[test]
    fn a_changed_file_resolves_inside_the_workspace_or_is_refused() {
        assert_eq!(
            change_target("/work", "src/a.rs").as_deref(),
            Some("/work/src/a.rs")
        );
        assert_eq!(
            change_target("/work", "/work/a.rs").as_deref(),
            Some("/work/a.rs")
        );
        assert_eq!(change_target("/work", "/etc/passwd"), None);
        assert_eq!(change_target("/work", "../etc/passwd"), None);
        assert_eq!(change_target("/work", "/work/../etc/passwd"), None);
    }

    #[test]
    fn stopping_a_subagent_sends_its_call_id_as_an_extension_notification() {
        assert_eq!(
            FromPanel::parse(r#"{"type":"stop_subagent","id":"c0"}"#),
            Some(FromPanel::StopSubagent("c0".to_owned()))
        );
        let line = session().stop_subagent("c0").expect("a session is open");
        let sent: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(sent["method"], "_praxis/cancel_subagent");
        assert_eq!(
            sent["params"],
            json!({"sessionId": "s1", "toolCallId": "c0"})
        );
        assert!(sent.get("id").is_none(), "a notification has no id: {sent}");
        assert_eq!(Client::start("/work").0.stop_subagent("c0"), None);
    }

    #[test]
    fn open_change_requests_carry_their_path_and_need_one() {
        assert_eq!(
            FromPanel::parse(r#"{"type":"open_change","path":"/work/a.rs","call":"c1"}"#),
            Some(FromPanel::OpenChange {
                path: "/work/a.rs".to_owned(),
                call: Some("c1".to_owned()),
            })
        );
        assert_eq!(FromPanel::parse(r#"{"type":"open_change"}"#), None);
    }

    #[test]
    fn panel_messages_parse_only_into_known_variants() {
        assert_eq!(
            FromPanel::parse(r#"{"type":"ready"}"#),
            Some(FromPanel::Ready)
        );
        assert_eq!(
            FromPanel::parse(r#"{"type":"prompt","text":"hi"}"#),
            Some(FromPanel::Prompt {
                text: "hi".to_owned(),
                file: None,
                files: vec![],
                problems: false
            })
        );
        assert_eq!(
            FromPanel::parse(
                r#"{"type":"prompt","text":"hi","file":"/work/a.rs","files":["/work/b.rs"],"problems":true}"#
            ),
            Some(FromPanel::Prompt {
                text: "hi".to_owned(),
                file: Some("/work/a.rs".to_owned()),
                files: vec!["/work/b.rs".to_owned()],
                problems: true
            })
        );
        assert_eq!(
            FromPanel::parse(r#"{"type":"add_file"}"#),
            Some(FromPanel::AddFile)
        );
        assert_eq!(
            FromPanel::parse(r#"{"type":"cancel"}"#),
            Some(FromPanel::Cancel)
        );
        assert_eq!(
            FromPanel::parse(r#"{"type":"permission","id":7,"option":"allow_once"}"#),
            Some(FromPanel::Permission(
                json!(7),
                Some("allow_once".to_owned())
            ))
        );
        assert_eq!(
            FromPanel::parse(r#"{"type":"permission","id":7,"option":null}"#),
            Some(FromPanel::Permission(json!(7), None))
        );
        assert_eq!(
            FromPanel::parse(r#"{"type":"dropped","uris":["file:///a.rs"]}"#),
            Some(FromPanel::Dropped(vec!["file:///a.rs".to_owned()]))
        );
        for bad in [
            r#"{"type":"prompt"}"#,
            r#"{"type":"prompt","text":"hi","files":[1]}"#,
            r#"{"type":"dropped"}"#,
            r#"{"type":"dropped","uris":["file:///a.rs",1]}"#,
            r#"{"type":"exec"}"#,
            r#"{"type":"permission","id":null,"option":"x"}"#,
            r#"{"type":"permission","id":{"n":7},"option":"x"}"#,
            "ready",
            "{",
        ] {
            assert_eq!(FromPanel::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_model_picker_comes_from_the_session_options_and_follows_a_switch() {
        let mut client = Client::start("/work").0;
        client.on_line(r#"{"jsonrpc":"2.0","id":0,"result":{}}"#);
        let opened = client.on_line(
            r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"s1","configOptions":[
                {"id":"model","name":"Model","category":"model","type":"select","currentValue":"m",
                 "options":[{"value":"m","name":"m"},{"value":"other","name":"Other"}]}]}}"#,
        );
        let picker = json!({"type": "models", "current": "m", "options": [
            {"value": "m", "name": "m"}, {"value": "other", "name": "Other"}]});
        assert_eq!(opened.models, Some(picker.clone()));

        assert_eq!(
            FromPanel::parse(r#"{"type":"model","key":"other"}"#),
            Some(FromPanel::Model("other".into()))
        );
        let switch: Value = serde_json::from_str(&client.set_model("other").unwrap()).unwrap();
        assert_eq!(switch["method"], "session/set_config_option");
        assert_eq!(switch["params"]["sessionId"], "s1");
        assert_eq!(switch["params"]["configId"], "model");
        assert_eq!(switch["params"]["value"], "other");

        let switched = r#"{"jsonrpc":"2.0","id":2,"result":{"configOptions":[
            {"id":"model","name":"Model","type":"select","currentValue":"other",
             "options":[{"value":"m","name":"m"},{"value":"other","name":"Other"}]}]}}"#;
        let picker = json!({"type": "models", "current": "other", "options": [
            {"value": "m", "name": "m"}, {"value": "other", "name": "Other"}]});
        assert_eq!(client.on_line(switched).models, Some(picker.clone()));

        let refused = r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"Invalid params","data":"no model `x`"}}"#;
        let reaction = client.on_line(refused);
        assert_eq!(
            reaction.show,
            Some(notice("Could not switch model: no model `x`"))
        );
        assert_eq!(reaction.models, Some(picker));
    }

    #[test]
    fn the_permission_mode_picker_is_offered_and_sent_for_the_session() {
        let mut client = Client::start("/work").0;
        client.on_line(r#"{"jsonrpc":"2.0","id":0,"result":{}}"#);
        let opened = client.on_line(
            r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"s1","configOptions":[
                {"id":"mode","name":"Permissions","category":"mode","type":"select","currentValue":"ask",
                 "options":[{"value":"read-only","name":"Read-only"},{"value":"ask","name":"Ask"}]}]}}"#,
        );
        let picker = json!({"type": "modes", "current": "ask", "options": [
            {"value": "read-only", "name": "Read-only"}, {"value": "ask", "name": "Ask"}]});
        assert_eq!(opened.modes, Some(picker.clone()));

        assert_eq!(
            FromPanel::parse(r#"{"type":"mode","key":"read-only"}"#),
            Some(FromPanel::PermissionMode("read-only".into()))
        );
        let switch: Value = serde_json::from_str(&client.set_mode("read-only").unwrap()).unwrap();
        assert_eq!(switch["method"], "session/set_config_option");
        assert_eq!(switch["params"]["configId"], "mode");
        assert_eq!(switch["params"]["value"], "read-only");

        let refused = r#"{"jsonrpc":"2.0","id":5,"error":{"code":-32602,"message":"Invalid params","data":"no mode `x`"}}"#;
        let reaction = client.on_line(refused);
        assert_eq!(
            reaction.show,
            Some(notice("Could not change permissions: no mode `x`"))
        );
        assert_eq!(reaction.modes, Some(picker));
        assert_eq!(reaction.models, None);
    }

    #[test]
    fn a_model_message_without_a_key_is_dropped() {
        assert_eq!(FromPanel::parse(r#"{"type":"model"}"#), None);
    }

    #[test]
    fn handshake_opens_a_session_before_prompts_are_sent() {
        let (mut client, init) = Client::start("/work");
        let init: Value = serde_json::from_str(&init).unwrap();
        assert_eq!(init["method"], "initialize");
        assert_eq!(
            init["params"]["clientCapabilities"]["fs"]["readTextFile"],
            true
        );
        assert_eq!(client.prompt("hi", &Context::default()), None);

        let reaction = client.on_line(r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1}}"#);
        let new = sent(&reaction);
        assert_eq!(new["method"], "session/new");
        assert_eq!(new["params"]["cwd"], "/work");
        assert!(new["params"]["mcpServers"].is_array());

        let reaction = client.on_line(r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"s1"}}"#);
        assert_eq!(reaction.show, Some(notice("ready")));
        let prompt: Value =
            serde_json::from_str(&client.prompt("hi", &Context::default()).unwrap()).unwrap();
        assert_eq!(prompt["method"], "session/prompt");
        assert_eq!(prompt["params"]["sessionId"], "s1");
        assert_eq!(prompt["params"]["prompt"][0]["text"], "hi");
    }

    #[test]
    fn mcp_list_and_toggle_round_trip_through_the_session() {
        let (mut client, _) = Client::start("/work");
        assert_eq!(client.mcp_servers(), None);
        client.on_line(r#"{"jsonrpc":"2.0","id":0,"result":{}}"#);
        let reaction = client.on_line(r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"s1"}}"#);
        assert!(reaction.session_ready);

        let list: Value = serde_json::from_str(&client.mcp_servers().unwrap()).unwrap();
        assert_eq!(list["method"], "_praxis/mcp_servers");
        assert_eq!(list["params"]["sessionId"], "s1");
        let answer = format!(r#"{{"jsonrpc":"2.0","id":{MCP_LIST},"result":{{"servers":[]}}}}"#);
        assert_eq!(client.on_line(&answer).mcp, Some(json!({"servers": []})));

        let toggle: Value =
            serde_json::from_str(&client.set_mcp_enabled("fs", false).unwrap()).unwrap();
        assert_eq!(toggle["method"], "_praxis/set_mcp_enabled");
        assert_eq!(toggle["params"]["name"], "fs");
        assert_eq!(toggle["params"]["enabled"], false);
        let done = format!(r#"{{"jsonrpc":"2.0","id":{MCP_TOGGLE},"result":{{}}}}"#);
        let reaction = client.on_line(&done);
        assert!(sent(&reaction)["method"] == "_praxis/mcp_servers");

        let approve: Value = serde_json::from_str(&client.approve_mcp("fs").unwrap()).unwrap();
        assert_eq!(approve["method"], "_praxis/approve_mcp_server");
        assert_eq!(approve["params"]["name"], "fs");
        let done = format!(r#"{{"jsonrpc":"2.0","id":{MCP_APPROVE},"result":{{}}}}"#);
        assert!(sent(&client.on_line(&done))["method"] == "_praxis/mcp_servers");
    }

    #[test]
    fn a_replayed_chat_reaches_the_panel_in_order() {
        let (mut client, _) = Client::start("/work");
        let replay = [
            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "go"}}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Reading."}}),
            json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "Read a.rs", "status": "pending"}),
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "completed",
                "content": [{"type": "content", "content": {"type": "text", "text": "fn a"}}]}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "done"}}),
        ];
        let shown: Vec<Value> = replay
            .into_iter()
            .filter_map(|update| {
                let line = json!({"jsonrpc": "2.0", "method": "session/update",
                    "params": {"sessionId": "s1", "update": update}});
                client.on_line(&line.to_string()).show
            })
            .collect();
        assert_eq!(
            shown,
            [
                json!({"type": "user", "text": "go"}),
                json!({"type": "text", "text": "Reading."}),
                json!({"type": "tool", "id": "c1", "title": "Read a.rs", "status": "pending", "output": null}),
                json!({"type": "tool", "id": "c1", "title": null, "status": "completed", "output": "fn a"}),
                json!({"type": "text", "text": "done"}),
            ]
        );
    }

    #[test]
    fn a_subagents_prompt_size_reaches_the_panel_for_its_agent_call() {
        let (mut client, _) = Client::start("/work");
        let usage = json!({"sessionUpdate": "tool_call_update", "toolCallId": "c0",
            "_meta": {"praxis": {"subagentUsage": {"promptTokens": 12000, "contextLength": 32000}}}});
        let line = json!({"jsonrpc": "2.0", "method": "session/update",
            "params": {"sessionId": "s1", "update": usage}});
        assert_eq!(
            client.on_line(&line.to_string()).show,
            Some(
                json!({"type": "subagent_usage", "id": "c0", "promptTokens": 12000, "contextLength": 32000})
            )
        );
    }

    #[test]
    fn a_subagents_text_update_reaches_the_panel_as_lines_to_append() {
        let (mut client, _) = Client::start("/work");
        let text = json!({"sessionUpdate": "tool_call_update", "toolCallId": "c0",
            "content": [{"type": "content", "content": {"type": "text", "text": "one\n"}}],
            "_meta": {"praxis": {"append": true}}});
        let line = json!({"jsonrpc": "2.0", "method": "session/update",
            "params": {"sessionId": "s1", "update": text}});
        assert_eq!(
            client.on_line(&line.to_string()).show,
            Some(
                json!({"type": "tool", "id": "c0", "title": null, "status": null,
                "output": "one\n", "append": true})
            )
        );
    }

    #[test]
    fn a_subagents_tool_calls_reach_the_panel_with_their_parent() {
        let (mut client, _) = Client::start("/work");
        let agent = json!({"sessionUpdate": "tool_call", "toolCallId": "c0", "title": "Agent",
            "status": "pending", "_meta": {"praxis": {"subagent": "general"}}});
        let child = json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "Read a.rs",
            "status": "pending", "_meta": {"praxis": {"parentToolCallId": "c0"}}});
        let shown: Vec<Value> = [agent, child]
            .into_iter()
            .filter_map(|update| {
                let line = json!({"jsonrpc": "2.0", "method": "session/update",
                    "params": {"sessionId": "s1", "update": update}});
                client.on_line(&line.to_string()).show
            })
            .collect();
        assert_eq!(
            shown,
            [
                json!({"type": "tool", "id": "c0", "title": "Agent", "status": "pending", "output": null, "subagent": "general"}),
                json!({"type": "tool", "id": "c1", "title": "Read a.rs", "status": "pending", "output": null, "parent": "c0"}),
            ]
        );
    }

    #[test]
    fn session_updates_and_stop_reasons_map_to_panel_messages() {
        let (mut client, _) = Client::start("/work");
        let mut on = |update: Value| {
            let line = json!({"jsonrpc": "2.0", "method": "session/update",
                "params": {"sessionId": "s1", "update": update}});
            client.on_line(&line.to_string()).show
        };
        let chunk = json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Hi"}});
        assert_eq!(on(chunk), Some(json!({"type": "text", "text": "Hi"})));
        let replayed = json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "fix it"}});
        assert_eq!(
            on(replayed),
            Some(json!({"type": "user", "text": "fix it"}))
        );
        let tool = json!({"sessionUpdate": "tool_call", "toolCallId": "t", "title": "Read a.rs", "status": "pending"});
        assert_eq!(
            on(tool),
            Some(
                json!({"type": "tool", "id": "t", "title": "Read a.rs", "status": "pending", "output": null})
            )
        );
        let finished = json!({"sessionUpdate": "tool_call_update", "toolCallId": "t", "status": "completed",
            "content": [{"type": "content", "content": {"type": "text", "text": "fn a"}},
                        {"type": "content", "content": {"type": "text", "text": "() {}"}}]});
        assert_eq!(
            on(finished),
            Some(
                json!({"type": "tool", "id": "t", "title": null, "status": "completed", "output": "fn a\n() {}"})
            )
        );
        let usage = json!({"sessionUpdate": "usage_update", "used": 5, "size": 9});
        assert_eq!(
            on(usage),
            Some(json!({"type": "usage", "used": 5, "size": 9}))
        );
        // The first prompt's id; ids below it belong to the handshake, list and load requests.
        let done = r#"{"jsonrpc":"2.0","id":6,"result":{"stopReason":"end_turn"}}"#;
        assert_eq!(
            client.on_line(done).show,
            Some(json!({"type": "done", "reason": "end_turn"}))
        );
        let failed = r#"{"jsonrpc":"2.0","id":6,"error":{"code":-32603,"message":"boom"}}"#;
        assert_eq!(
            client.on_line(failed).show,
            Some(json!({"type": "done", "reason": "error: boom"}))
        );
    }

    #[test]
    fn available_commands_reach_the_panel_with_their_source() {
        let (mut client, _) = Client::start("/work");
        let line = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"ponytail:lite","description":"Lite mode","_meta":{"source":"ponytail"}},{"name":"deploy","description":"Ship it","_meta":{"source":"/home/u/.claude/skills/deploy"}}]}}}"#;
        assert_eq!(
            client.on_line(line).show,
            Some(json!({"type": "commands", "commands": [
                {"name": "ponytail:lite", "description": "Lite mode", "source": "ponytail"},
                {"name": "deploy", "description": "Ship it", "source": "/home/u/.claude/skills/deploy"}
            ]}))
        );
    }

    const ASK: &str = r#"{"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{"sessionId":"s1","toolCall":{"toolCallId":"t","title":"Bash"},"options":[{"optionId":"allow_once","name":"Allow once","kind":"allow_once"},{"optionId":"reject_once","name":"Reject","kind":"reject_once"}]}}"#;

    fn session() -> Client {
        let (mut client, _) = Client::start("/work");
        client.on_line(r#"{"jsonrpc":"2.0","id":0,"result":{}}"#);
        client.on_line(r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"s1"}}"#);
        client
    }

    #[test]
    fn permission_requests_prompt_in_the_panel_and_send_the_chosen_option() {
        let mut client = session();
        let reaction = client.on_line(ASK);
        assert_eq!(reaction.send, None);
        assert_eq!(
            reaction.show,
            Some(
                json!({"type": "permission", "id": 7, "title": "Bash", "options": [
                {"id": "allow_once", "name": "Allow once"}, {"id": "reject_once", "name": "Reject"}]})
            )
        );
        // The webview is untrusted: unknown requests and unoffered options are refused.
        assert_eq!(client.answer(&json!(8), Some("allow_once")), None);
        assert_eq!(client.answer(&json!(7), Some("allow_always")), None);
        let answer: Value =
            serde_json::from_str(&client.answer(&json!(7), Some("allow_once")).unwrap()).unwrap();
        assert_eq!(answer["id"], 7);
        assert_eq!(
            answer["result"]["outcome"],
            json!({"outcome": "selected", "optionId": "allow_once"})
        );
        assert_eq!(
            client.answer(&json!(7), Some("allow_once")),
            None,
            "answered once"
        );
    }

    #[test]
    fn permission_requests_with_string_ids_reach_the_panel_and_are_answered() {
        // agent-client-protocol 5 numbers its requests with strings.
        let mut client = session();
        let reaction = client.on_line(&ASK.replace(r#""id":7"#, r#""id":"r-7""#));
        assert_eq!(reaction.send, None);
        assert_eq!(
            reaction.show.as_ref().map(|s| &s["id"]),
            Some(&json!("r-7"))
        );
        let Some(FromPanel::Permission(id, option)) =
            FromPanel::parse(r#"{"type":"permission","id":"r-7","option":"reject_once"}"#)
        else {
            panic!("the panel's answer must parse");
        };
        let answer: Value =
            serde_json::from_str(&client.answer(&id, option.as_deref()).unwrap()).unwrap();
        assert_eq!(answer["id"], "r-7");
        assert_eq!(answer["result"]["outcome"]["optionId"], "reject_once");
    }

    #[test]
    fn a_permission_with_a_diff_also_asks_the_editor_to_review_it() {
        let mut client = session();
        let ask = json!({"jsonrpc": "2.0", "id": 7, "method": "session/request_permission",
            "params": {"sessionId": "s1", "toolCall": {"toolCallId": "t", "title": "Edit",
                "content": [{"type": "diff", "path": "/work/a.txt", "oldText": "one\n", "newText": "two\n"}]},
            "options": [{"optionId": "allow_once", "name": "Allow once", "kind": "allow_once"}]}});
        let reaction = client.on_line(&ask.to_string());
        assert_eq!(
            reaction.review,
            Some(Review {
                title: "Edit: /work/a.txt".to_owned(),
                old_text: "one\n".to_owned(),
                new_text: "two\n".to_owned(),
            })
        );
        assert_eq!(reaction.show.as_ref().unwrap()["type"], "permission");
        // A request without a diff opens no review, and a new file has an empty old side.
        assert_eq!(client.on_line(ASK).review, None);
        let create = json!({"jsonrpc": "2.0", "id": 8, "method": "session/request_permission",
            "params": {"sessionId": "s1", "toolCall": {"toolCallId": "u", "title": "Write",
                "content": [{"type": "diff", "path": "/work/b.txt", "newText": "new\n"}]},
            "options": []}});
        assert_eq!(
            client.on_line(&create.to_string()).review.unwrap().old_text,
            ""
        );
    }

    #[test]
    fn stop_cancels_the_session_and_answers_pending_permissions_cancelled() {
        let mut client = session();
        client.on_line(ASK);
        let lines: Vec<Value> = client
            .cancel()
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines[0]["method"], "session/cancel");
        assert_eq!(lines[0]["params"]["sessionId"], "s1");
        assert_eq!(lines[1]["id"], 7);
        assert_eq!(lines[1]["result"]["outcome"]["outcome"], "cancelled");
        assert_eq!(lines.len(), 2);
        assert_eq!(client.answer(&json!(7), Some("allow_once")), None);
    }

    #[test]
    fn panel_answers_without_an_option_are_cancelled() {
        let mut client = session();
        client.on_line(ASK);
        let answer: Value = serde_json::from_str(&client.answer(&json!(7), None).unwrap()).unwrap();
        assert_eq!(answer["result"]["outcome"]["outcome"], "cancelled");
    }

    #[test]
    fn file_reads_are_answered_only_inside_the_workspace() {
        let mut client = session();
        let read = |path: &str| {
            json!({"jsonrpc": "2.0", "id": 9, "method": "fs/read_text_file",
                "params": {"sessionId": "s1", "path": path}})
            .to_string()
        };
        assert_eq!(
            client.on_line(&read("/work/src/a.rs")).read,
            Some(FileRead {
                id: json!(9),
                path: "/work/src/a.rs".into()
            })
        );
        for outside in [
            "/etc/passwd",
            "/workx/a.rs",
            "/work/../etc/passwd",
            "src/a.rs",
            "",
        ] {
            let reaction = client.on_line(&read(outside));
            assert_eq!(reaction.read, None, "{outside}");
            let refusal = sent(&reaction);
            assert_eq!(refusal["id"], 9);
            assert_eq!(refusal["error"]["code"], -32602);
        }
    }

    #[test]
    fn diagnostics_are_asked_inside_the_workspace_and_answer_only_errors() {
        let mut client = session();
        let ask = |path: &str| {
            json!({"jsonrpc": "2.0", "id": "d1", "method": "_praxis/diagnostics",
                "params": {"sessionId": "s1", "path": path}})
            .to_string()
        };
        assert_eq!(
            client.on_line(&ask("/work/src/a.rs")).diagnostics,
            Some(FileRead {
                id: json!("d1"),
                path: "/work/src/a.rs".into()
            })
        );
        assert_eq!(client.on_line(&ask("/etc/passwd")).diagnostics, None);
        let items = [
            Problem {
                line: 3,
                severity: "error",
                message: "expected `;`".into(),
            },
            Problem {
                line: 4,
                severity: "warning",
                message: "unused".into(),
            },
        ];
        let reply: Value = serde_json::from_str(&diagnostics_reply(json!("d1"), &items)).unwrap();
        assert_eq!(
            reply,
            json!({"jsonrpc": "2.0", "id": "d1", "result": {"problems":
                [{"line": 3, "severity": "error", "message": "expected `;`"}]}})
        );
    }

    #[test]
    fn file_replies_carry_the_text_or_an_error() {
        let ok: Value = serde_json::from_str(&file_reply(json!(9), Some("hi"))).unwrap();
        assert_eq!(
            ok,
            json!({"jsonrpc": "2.0", "id": 9, "result": {"content": "hi"}})
        );
        let failed: Value = serde_json::from_str(&file_reply(json!(9), None)).unwrap();
        assert_eq!(failed["id"], 9);
        assert_eq!(failed["error"]["code"], -32002);
    }

    #[test]
    fn a_startup_error_is_shown_as_a_notice_with_its_detail() {
        // ACP puts the reason in `data`; `message` is only the generic "Internal error".
        let mut client = Client::start("/work").0;
        let failed = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"Internal error","data":"LM Studio has no model loaded."}}"#;
        client.on_line(r#"{"jsonrpc":"2.0","id":0,"result":{}}"#);
        assert_eq!(
            client.on_line(failed).show,
            Some(notice(
                "Praxis could not start: LM Studio has no model loaded."
            ))
        );
        assert_eq!(client.prompt("hi", &Context::default()), None);
    }

    #[test]
    fn prompts_carry_the_file_selection_and_problems_as_blocks() {
        let context = Context {
            files: vec!["/work/a.rs".into(), "/etc/passwd".into()],
            selection: Some(Selection {
                path: "/work/a.rs".into(),
                text: "let x = 1;".into(),
                start_line: 3,
                end_line: 4,
            }),
            problems: Some(Problems {
                path: "/work/a.rs".into(),
                items: vec![Problem {
                    line: 3,
                    severity: severity(0),
                    message: "expected `;`".into(),
                }],
            }),
        };
        let blocks = prompt_blocks("explain", &context, "/work");
        assert_eq!(blocks.len(), 4, "the outside file is dropped");
        assert_eq!(blocks[0], json!({"type": "text", "text": "explain"}));
        assert_eq!(
            blocks[1],
            json!({"type": "resource_link", "uri": "file:///work/a.rs", "name": "a.rs"})
        );
        assert_eq!(blocks[2]["type"], "resource");
        assert_eq!(
            blocks[2]["resource"],
            json!({"uri": "file:///work/a.rs#L3-4", "text": "let x = 1;"})
        );
        assert_eq!(
            blocks[3]["text"],
            "Problems in /work/a.rs:\n- line 3 error: expected `;`"
        );
    }

    #[test]
    fn a_selection_outside_the_workspace_or_no_problems_add_no_blocks() {
        let outside = Context {
            selection: Some(Selection {
                path: "/home/me/x.rs".into(),
                text: "secret".into(),
                start_line: 1,
                end_line: 1,
            }),
            problems: Some(Problems {
                path: "/work/a.rs".into(),
                items: vec![],
            }),
            ..Context::default()
        };
        assert_eq!(prompt_blocks("hi", &outside, "/work").len(), 1);
        assert_eq!(severity(9), "hint");
    }

    #[test]
    fn a_tool_card_carries_its_key_argument_when_the_agent_sent_input() {
        let call = json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "Edit a.rs",
            "status": "in_progress", "rawInput": {"file_path": "/w/a.rs", "old_string": "x"}});
        assert_eq!(update(&call).show.unwrap()["arg"], "/w/a.rs");
        let shell = json!({"sessionUpdate": "tool_call", "toolCallId": "c2", "title": "Bash",
            "status": "pending", "rawInput": {"command": "cargo test"}});
        assert_eq!(update(&shell).show.unwrap()["arg"], "cargo test");
        let bare = json!({"sessionUpdate": "tool_call", "toolCallId": "c3", "title": "Glob",
            "status": "pending", "rawInput": {"limit": 3}});
        assert!(update(&bare).show.unwrap().get("arg").is_none());
    }

    #[test]
    fn lines_are_split_across_chunks() {
        let mut lines = Lines::default();
        assert!(lines.push("{\"a\"").is_empty());
        assert_eq!(
            lines.push(":1}\n{\"b\":2}\n{"),
            vec!["{\"a\":1}", "{\"b\":2}"]
        );
        assert_eq!(lines.push("}\n"), vec!["{}"]);
    }

    #[test]
    fn a_resumed_session_is_loaded_before_prompts_go_to_it() {
        let (mut client, init) = Client::resume("/w/a", "s9");
        assert!(init.contains("initialize"));
        let load = sent(&client.on_line(r#"{"jsonrpc":"2.0","id":0,"result":{}}"#));
        assert_eq!(load["method"], "session/load");
        assert_eq!(load["params"]["sessionId"], "s9");
        assert_eq!(load["params"]["cwd"], "/w/a");
        // Until the load answer arrives, no prompt can go out.
        assert!(client.prompt("hi", &Context::default()).is_none());
        client.on_line(&json!({"jsonrpc": "2.0", "id": load["id"], "result": {}}).to_string());
        let prompt = sent(&Reaction {
            send: client.prompt("hi", &Context::default()),
            ..Reaction::default()
        });
        assert_eq!(prompt["params"]["sessionId"], "s9");
    }

    #[test]
    fn sessions_json_maps_to_items_with_a_placeholder_for_untitled() {
        let items = session_items(r#"[{"id":"new","title":"second"},{"id":"old","title":""}]"#);
        assert_eq!(
            items,
            Ok(vec![
                SessionItem {
                    id: "new".to_owned(),
                    label: "second".to_owned(),
                    folder: None,
                },
                SessionItem {
                    id: "old".to_owned(),
                    label: "Untitled session".to_owned(),
                    folder: None,
                },
            ])
        );
    }

    #[test]
    fn sessions_json_carries_each_session_folder() {
        let items = session_items(
            r#"[{"id":"a","title":"t","folder":"Work"},{"id":"b","title":"t","folder":null}]"#,
        );
        let folders: Vec<Option<String>> = items.unwrap().into_iter().map(|i| i.folder).collect();
        assert_eq!(folders, [Some("Work".to_owned()), None]);
    }

    #[test]
    fn folder_names_are_listed_once_each_in_order() {
        let item = |folder: Option<&str>| SessionItem {
            id: "x".to_owned(),
            label: "x".to_owned(),
            folder: folder.map(str::to_owned),
        };
        let items = [
            item(Some("Work")),
            item(None),
            item(Some("Home")),
            item(Some("Work")),
        ];
        assert_eq!(folder_names(&items), ["Home", "Work"]);
    }

    #[test]
    fn moving_a_session_passes_only_well_formed_ids_to_the_cli() {
        assert_eq!(
            folder_args("1700000000-42", "-Work"),
            Some(vec![
                "sessions".to_owned(),
                "--folder".to_owned(),
                "1700000000-42".to_owned(),
                "-Work".to_owned(),
            ])
        );
        assert_eq!(folder_args("../secret", "Work"), None);
        assert_eq!(folder_args("", "Work"), None);
    }

    #[test]
    fn a_malformed_sessions_answer_fails_the_whole_listing() {
        assert!(session_items(r#"{"id":"x"}"#).is_err());
        assert!(session_items(r#"[{"title":"no id"}]"#).is_err());
    }

    #[test]
    fn deleting_a_session_passes_only_well_formed_ids_to_the_cli() {
        assert_eq!(
            delete_args("1700000000-42"),
            Some(vec![
                "sessions".to_owned(),
                "--delete=1700000000-42".to_owned()
            ])
        );
        assert_eq!(delete_args("../secret"), None);
        assert_eq!(delete_args("a/b"), None);
        assert_eq!(delete_args("..\\secret"), None);
        assert_eq!(delete_args(""), None);
        assert_eq!(delete_args("--json"), None);
    }
}
