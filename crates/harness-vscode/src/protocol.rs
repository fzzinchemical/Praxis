//! ACP client framing and the panel message mapping, kept free of `JsValue` so it tests natively.
//!
//! Lines are framed by hand with `serde_json` rather than the `agent-client-protocol` crate (SPEC §11).

use serde_json::{Value, json};

/// A message from the webview. The webview is untrusted, so anything else is dropped.
#[derive(Debug, PartialEq)]
pub enum FromPanel {
    Ready,
    Prompt(String),
}

impl FromPanel {
    pub fn parse(message: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(message).ok()?;
        match value["type"].as_str()? {
            "ready" => Some(Self::Ready),
            "prompt" => Some(Self::Prompt(value["text"].as_str()?.to_owned())),
            _ => None,
        }
    }
}

/// What one line from `harness acp` asks the extension to do.
#[derive(Debug, Default, PartialEq)]
pub struct Reaction {
    /// Text for the panel.
    pub show: Option<String>,
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

    pub fn on_line(&mut self, line: &str) -> Reaction {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return show(format!("[unreadable agent output: {line}]\n"));
        };
        if let Some(error) = message.get("error") {
            let text = error["message"].as_str().unwrap_or("unknown error");
            return show(format!("[error: {text}]\n"));
        }
        match (message["method"].as_str(), &message["id"]) {
            (Some("session/update"), _) => self.update(&message["params"]["update"]),
            // shortcut: permission prompts come with the chat UI in T5.3; until then every request is refused.
            (Some("session/request_permission"), id) => Reaction {
                show: Some("[permission refused: prompts arrive in T5.3]\n".to_owned()),
                send: Some(json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": {"outcome": "cancelled"}}}).to_string()),
            },
            (Some(_), _) => Reaction::default(),
            (None, id) => self.response(id.as_u64(), &message["result"]),
        }
    }

    fn update(&self, update: &Value) -> Reaction {
        match update["sessionUpdate"].as_str() {
            Some("agent_message_chunk") => match update["content"]["text"].as_str() {
                Some(text) => show(text.to_owned()),
                None => Reaction::default(),
            },
            Some("tool_call") => show(format!(
                "\n[tool: {}]\n",
                update["title"].as_str().unwrap_or("?")
            )),
            _ => Reaction::default(),
        }
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
                show("[ready]\n".to_owned())
            }
            _ => match result["stopReason"].as_str() {
                Some(reason) => show(format!("\n[{reason}]\n")),
                None => Reaction::default(),
            },
        }
    }
}

fn show(text: String) -> Reaction {
    Reaction {
        show: Some(text),
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
        for bad in [r#"{"type":"prompt"}"#, r#"{"type":"exec"}"#, "ready", "{"] {
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
        assert_eq!(reaction.show.as_deref(), Some("[ready]\n"));
        let prompt: Value = serde_json::from_str(&client.prompt("hi").unwrap()).unwrap();
        assert_eq!(prompt["method"], "session/prompt");
        assert_eq!(prompt["params"]["sessionId"], "s1");
        assert_eq!(prompt["params"]["prompt"][0]["text"], "hi");
    }

    #[test]
    fn session_updates_and_stop_reasons_map_to_panel_text() {
        let (mut client, _) = Client::start("/work");
        let chunk = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Hello"}}}}"#;
        assert_eq!(client.on_line(chunk).show.as_deref(), Some("Hello"));
        let tool = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"tool_call","toolCallId":"t","title":"Read a.rs"}}}"#;
        assert_eq!(
            client.on_line(tool).show.as_deref(),
            Some("\n[tool: Read a.rs]\n")
        );
        let done = r#"{"jsonrpc":"2.0","id":2,"result":{"stopReason":"end_turn"}}"#;
        assert_eq!(client.on_line(done).show.as_deref(), Some("\n[end_turn]\n"));
        let failed = r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32603,"message":"boom"}}"#;
        assert_eq!(
            client.on_line(failed).show.as_deref(),
            Some("[error: boom]\n")
        );
    }

    #[test]
    fn permission_requests_are_answered_cancelled_so_the_turn_never_hangs() {
        let (mut client, _) = Client::start("/work");
        let ask = r#"{"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{}}"#;
        let answer = sent(&client.on_line(ask));
        assert_eq!(answer["id"], 7);
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
