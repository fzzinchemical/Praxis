//! Helpers shared by the praxis-cli integration test binaries.

// Clippy's `allow-unwrap-in-tests` only applies to helpers in files that start with this.
#![cfg(test)]

use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

pub const BIN: &str = env!("CARGO_BIN_EXE_praxis");
pub const CHAT: &str = "/v1/chat/completions";
pub const LOAD: &str = "/api/v1/models/load";

pub struct Script(Vec<String>, AtomicUsize);

impl Respond for Script {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let i = self.1.fetch_add(1, Ordering::SeqCst);
        ResponseTemplate::new(200)
            .set_body_raw(self.0[i.min(self.0.len() - 1)].clone(), "text/event-stream")
    }
}

pub fn sse(chunks: &[Value]) -> String {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    body
}

pub fn text_reply(text: &str) -> String {
    sse(&[
        json!({"choices": [{"delta": {"content": text}, "finish_reason": null}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
    ])
}

pub fn tool_reply(name: &str, args: &Value) -> String {
    sse(&[
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1",
            "function": {"name": name, "arguments": args.to_string()}}]},
            "finish_reason": null}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

pub fn workspace() -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("praxis-cli-{}-{id}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// A server knowing model `m`, with instance `m:1` loaded if `loaded_context` is given.
pub async fn server(loaded_context: Option<u32>, chat: ResponseOrScript) -> MockServer {
    let server = MockServer::start().await;
    let instances: Vec<Value> = loaded_context
        .iter()
        .map(|c| json!({"id": "m:1", "config": {"context_length": c}}))
        .collect();
    Mock::given(method("GET"))
        .and(path("/api/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": [{
            "key": "m", "type": "llm", "max_context_length": 99999,
            "loaded_instances": instances,
            "capabilities": {"trained_for_tool_use": true},
        }]})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(LOAD))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "instance_id": "m:2", "load_config": {"context_length": 32768},
        })))
        .mount(&server)
        .await;
    let chat_mock = Mock::given(method("POST")).and(path(CHAT));
    match chat {
        ResponseOrScript::Script(bodies) => {
            chat_mock
                .respond_with(Script(bodies, AtomicUsize::new(0)))
                .mount(&server)
                .await;
        }
        ResponseOrScript::Hang => {
            chat_mock
                .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
                .mount(&server)
                .await;
        }
    }
    server
}

pub enum ResponseOrScript {
    Script(Vec<String>),
    Hang,
}
// `Hang` is unused in binaries that never hang a reply; the glob import still exposes it.
#[allow(unused_imports)]
pub use ResponseOrScript::{Hang, Script as Replies};

pub fn command(server_url: &str, dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(BIN);
    command
        .current_dir(dir)
        // Keeps the developer's own user config and ~/.claude memory out of the tests.
        .env("HOME", dir.join("home"))
        .env("XDG_CONFIG_HOME", dir.join("xdg"))
        .env("XDG_DATA_HOME", dir.join("xdg-data"))
        // Standalone skills read ~/.claude, so the developer's own home must not leak in.
        .env("HOME", dir)
        .args([
            "--config-override",
            &format!("lmstudio.base_url={server_url}"),
        ])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

pub async fn run(server: &MockServer, dir: &Path, args: &[&str]) -> Output {
    command(&server.uri(), dir, args).output().await.unwrap()
}

pub fn out(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn err(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

pub async fn requests_to(server: &MockServer, route: &str) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == route)
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

/// `script` gives the binary a real terminal, the only way to reach the REPL and approvals.
pub async fn in_terminal(url: &str, dir: &Path, args: &str, typed: &str) -> Output {
    let cmd = format!("'{BIN}' --config-override lmstudio.base_url={url} {args}");
    let mut child = Command::new("script")
        .args(["-qec", &cmd, "/dev/null"])
        .current_dir(dir)
        .env("HOME", dir.join("home"))
        .env("XDG_CONFIG_HOME", dir.join("xdg"))
        .env("XDG_DATA_HOME", dir.join("xdg-data"))
        // Standalone skills read ~/.claude, so the developer's own home must not leak in.
        .env("HOME", dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    // Held open until exit: `script` ends the session when its stdin closes.
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(typed.as_bytes()).await.unwrap();
    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    drop(stdin);
    output
}
