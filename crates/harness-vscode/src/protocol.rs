//! ACP client framing and the panel message mapping, kept free of `JsValue` so it tests natively.
//!
//! Lines are framed by hand with `serde_json` rather than the `agent-client-protocol` crate (SPEC §11).

use serde_json::{Value, json};

/// A message from the webview. The webview is untrusted, so anything else is dropped.
#[derive(Debug, PartialEq)]
pub enum FromPanel {
    Ready,
    Prompt(String),
    /// The stop button.
    Cancel,
    /// A permission request id and the chosen option, `None` meaning dismissed.
    Permission(u64, Option<String>),
}

impl FromPanel {
    pub fn parse(message: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(message).ok()?;
        match value["type"].as_str()? {
            "ready" => Some(Self::Ready),
            "prompt" => Some(Self::Prompt(value["text"].as_str()?.to_owned())),
            "cancel" => Some(Self::Cancel),
            "permission" => Some(Self::Permission(
                value["id"].as_u64()?,
                value["option"].as_str().map(str::to_owned),
            )),
            _ => None,
        }
    }
}

/// A line of status text for the panel.
pub fn notice(text: &str) -> Value {
    json!({"type": "notice", "text": text})
}

/// What one line from `harness acp` asks the extension to do.
#[derive(Debug, Default, PartialEq)]
pub struct Reaction {
    /// A typed message for the panel (`ui/src/lib.rs` handles each `type`).
    pub show: Option<Value>,
    /// A line for the agent's stdin.
    pub send: Option<String>,
}

const INITIALIZE: u64 = 0;
const NEW_SESSION: u64 = 1;

/// One ACP connection: `initialize`, then `session/new`, then prompts.
pub struct Client {
    cwd: String,
    session: Option<String>,
    next_id: u64,
    /// Open permission requests: id and the option ids offered.
    pending: Vec<(u64, Vec<String>)>,
}

impl Client {
    /// Returns the client and the `initialize` line to send first.
    pub fn start(cwd: &str) -> (Self, String) {
        let line = request(
            INITIALIZE,
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        );
        let client = Self {
            cwd: cwd.to_owned(),
            session: None,
            next_id: NEW_SESSION + 1,
            pending: Vec::new(),
        };
        (client, line)
    }

    /// The `session/prompt` line, or `None` while no session is open yet.
    pub fn prompt(&mut self, text: &str) -> Option<String> {
        let session = self.session.as_deref()?;
        let params = json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]});
        let line = request(self.next_id, "session/prompt", params);
        self.next_id += 1;
        Some(line)
    }

    /// The answer to a pending permission request, or `None` if the panel names one not offered.
    pub fn answer(&mut self, id: u64, option: Option<&str>) -> Option<String> {
        let index = self.pending.iter().position(|(pending, options)| {
            *pending == id && option.is_none_or(|option| options.iter().any(|o| o == option))
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
        let ids: Vec<u64> = self.pending.iter().map(|(id, _)| *id).collect();
        let answers = ids.into_iter().filter_map(|id| self.answer(id, None));
        std::iter::once(cancel.to_string()).chain(answers).collect()
    }

    pub fn on_line(&mut self, line: &str) -> Reaction {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return show(notice(&format!("unreadable agent output: {line}")));
        };
        if let Some(error) = message.get("error") {
            let text = error["message"].as_str().unwrap_or("unknown error");
            return show(json!({"type": "done", "reason": format!("error: {text}")}));
        }
        match (message["method"].as_str(), &message["id"]) {
            (Some("session/update"), _) => update(&message["params"]["update"]),
            (Some("session/request_permission"), id) => self.permission(id, &message["params"]),
            (Some(_), _) => Reaction::default(),
            (None, id) => self.response(id.as_u64(), &message["result"]),
        }
    }

    fn permission(&mut self, id: &Value, params: &Value) -> Reaction {
        let Some(id) = id.as_u64() else {
            // The panel can only name numeric ids, so refuse rather than leave the turn hanging.
            let send = json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": {"outcome": "cancelled"}}});
            return Reaction {
                show: Some(notice("permission refused: unsupported request id")),
                send: Some(send.to_string()),
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
        self.pending.push((id, ids.collect()));
        let title = params["toolCall"]["title"].as_str().unwrap_or("a tool");
        show(json!({"type": "permission", "id": id, "title": title, "options": options}))
    }

    fn response(&mut self, id: Option<u64>, result: &Value) -> Reaction {
        match id {
            Some(INITIALIZE) => Reaction {
                show: None,
                send: Some(request(
                    NEW_SESSION,
                    "session/new",
                    json!({"cwd": self.cwd, "mcpServers": []}),
                )),
            },
            Some(NEW_SESSION) => {
                self.session = result["sessionId"].as_str().map(str::to_owned);
                show(notice("ready"))
            }
            _ => match result["stopReason"].as_str() {
                Some(reason) => show(json!({"type": "done", "reason": reason})),
                None => Reaction::default(),
            },
        }
    }
}

fn update(update: &Value) -> Reaction {
    match update["sessionUpdate"].as_str() {
        Some("agent_message_chunk") => match update["content"]["text"].as_str() {
            Some(text) => show(json!({"type": "text", "text": text})),
            None => Reaction::default(),
        },
        Some("tool_call" | "tool_call_update") => {
            let texts: Vec<&str> = update["content"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter_map(|item| item["content"]["text"].as_str())
                .collect();
            let output = (!texts.is_empty()).then(|| texts.join("\n"));
            show(
                json!({"type": "tool", "id": update["toolCallId"], "title": update["title"],
                "status": update["status"], "output": output}),
            )
        }
        Some("usage_update") => {
            show(json!({"type": "usage", "used": update["used"], "size": update["size"]}))
        }
        _ => Reaction::default(),
    }
}

fn show(message: Value) -> Reaction {
    Reaction {
        show: Some(message),
        send: None,
    }
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

    fn sent(reaction: &Reaction) -> Value {
        serde_json::from_str(reaction.send.as_deref().unwrap_or("null")).unwrap()
    }

    #[test]
    fn panel_messages_parse_only_into_known_variants() {
        assert_eq!(
            FromPanel::parse(r#"{"type":"ready"}"#),
            Some(FromPanel::Ready)
        );
        assert_eq!(
            FromPanel::parse(r#"{"type":"prompt","text":"hi"}"#),
            Some(FromPanel::Prompt("hi".to_owned()))
        );
        assert_eq!(
            FromPanel::parse(r#"{"type":"cancel"}"#),
            Some(FromPanel::Cancel)
        );
        assert_eq!(
            FromPanel::parse(r#"{"type":"permission","id":7,"option":"allow_once"}"#),
            Some(FromPanel::Permission(7, Some("allow_once".to_owned())))
        );
        assert_eq!(
            FromPanel::parse(r#"{"type":"permission","id":7,"option":null}"#),
            Some(FromPanel::Permission(7, None))
        );
        for bad in [
            r#"{"type":"prompt"}"#,
            r#"{"type":"exec"}"#,
            r#"{"type":"permission","id":"7","option":"x"}"#,
            "ready",
            "{",
        ] {
            assert_eq!(FromPanel::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn handshake_opens_a_session_before_prompts_are_sent() {
        let (mut client, init) = Client::start("/work");
        let init: Value = serde_json::from_str(&init).unwrap();
        assert_eq!(init["method"], "initialize");
        assert_eq!(client.prompt("hi"), None);

        let reaction = client.on_line(r#"{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1}}"#);
        let new = sent(&reaction);
        assert_eq!(new["method"], "session/new");
        assert_eq!(new["params"]["cwd"], "/work");
        assert!(new["params"]["mcpServers"].is_array());

        let reaction = client.on_line(r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"s1"}}"#);
        assert_eq!(reaction.show, Some(notice("ready")));
        let prompt: Value = serde_json::from_str(&client.prompt("hi").unwrap()).unwrap();
        assert_eq!(prompt["method"], "session/prompt");
        assert_eq!(prompt["params"]["sessionId"], "s1");
        assert_eq!(prompt["params"]["prompt"][0]["text"], "hi");
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
        let done = r#"{"jsonrpc":"2.0","id":2,"result":{"stopReason":"end_turn"}}"#;
        assert_eq!(
            client.on_line(done).show,
            Some(json!({"type": "done", "reason": "end_turn"}))
        );
        let failed = r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32603,"message":"boom"}}"#;
        assert_eq!(
            client.on_line(failed).show,
            Some(json!({"type": "done", "reason": "error: boom"}))
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
        assert_eq!(client.answer(8, Some("allow_once")), None);
        assert_eq!(client.answer(7, Some("allow_always")), None);
        let answer: Value =
            serde_json::from_str(&client.answer(7, Some("allow_once")).unwrap()).unwrap();
        assert_eq!(answer["id"], 7);
        assert_eq!(
            answer["result"]["outcome"],
            json!({"outcome": "selected", "optionId": "allow_once"})
        );
        assert_eq!(client.answer(7, Some("allow_once")), None, "answered once");
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
        assert_eq!(client.answer(7, Some("allow_once")), None);
    }

    #[test]
    fn panel_answers_without_an_option_are_cancelled() {
        let mut client = session();
        client.on_line(ASK);
        let answer: Value = serde_json::from_str(&client.answer(7, None).unwrap()).unwrap();
        assert_eq!(answer["result"]["outcome"]["outcome"], "cancelled");
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
}
