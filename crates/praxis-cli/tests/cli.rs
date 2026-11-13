//! Runs the real binary against a fake LM Studio (SPEC §11, §14).

// Lets clippy treat helper functions here as test code for `allow-unwrap-in-tests`.
#![cfg(test)]

use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const BIN: &str = env!("CARGO_BIN_EXE_praxis");
const CHAT: &str = "/v1/chat/completions";
const LOAD: &str = "/api/v1/models/load";

struct Script(Vec<String>, AtomicUsize);

impl Respond for Script {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let i = self.1.fetch_add(1, Ordering::SeqCst);
        ResponseTemplate::new(200)
            .set_body_raw(self.0[i.min(self.0.len() - 1)].clone(), "text/event-stream")
    }
}

fn sse(chunks: &[Value]) -> String {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    body
}

fn text_reply(text: &str) -> String {
    sse(&[
        json!({"choices": [{"delta": {"content": text}, "finish_reason": null}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
    ])
}

fn tool_reply(name: &str, args: &Value) -> String {
    sse(&[
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1",
            "function": {"name": name, "arguments": args.to_string()}}]},
            "finish_reason": null}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

fn workspace() -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("praxis-cli-{}-{id}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// A server knowing model `m`, with instance `m:1` loaded if `loaded_context` is given.
async fn server(loaded_context: Option<u32>, chat: ResponseOrScript) -> MockServer {
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

enum ResponseOrScript {
    Script(Vec<String>),
    Hang,
}
use ResponseOrScript::{Hang, Script as Replies};

fn command(server_url: &str, dir: &Path, args: &[&str]) -> Command {
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

async fn run(server: &MockServer, dir: &Path, args: &[&str]) -> Output {
    command(&server.uri(), dir, args).output().await.unwrap()
}

fn out(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn err(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

async fn requests_to(server: &MockServer, route: &str) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == route)
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn one_shot_prints_the_reply_and_addresses_the_loaded_instance() {
    let server = server(Some(32768), Replies(vec![text_reply("hello there")])).await;

    let output = run(&server, &workspace(), &["--model", "m", "say hi"]).await;

    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    assert_eq!(out(&output).trim(), "hello there");
    let chats = requests_to(&server, CHAT).await;
    assert_eq!(chats[0]["model"], "m:1");
    assert_eq!(chats[0]["messages"][0]["role"], "system");
    assert_eq!(chats[0]["messages"][1]["content"], "say hi");
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_model_flag_it_uses_what_is_loaded() {
    let server = server(Some(4096), Replies(vec![text_reply("ok")])).await;

    let output = run(&server, &workspace(), &["hi"]).await;

    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    assert_eq!(requests_to(&server, CHAT).await[0]["model"], "m:1");
    assert!(requests_to(&server, LOAD).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn takes_the_prompt_from_stdin() {
    let server = server(Some(32768), Replies(vec![text_reply("ok")])).await;
    let mut child = command(&server.uri(), &workspace(), &["--model", "m"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"from a pipe\n").await.unwrap();
    drop(stdin);

    let output = child.wait_with_output().await.unwrap();

    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    assert_eq!(
        requests_to(&server, CHAT).await[0]["messages"][1]["content"],
        "from a pipe\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn non_interactive_runs_never_write_or_run_commands() {
    let dir = workspace();
    let target = dir.join("written.txt");
    let marker = dir.join("marker");
    let server = server(
        Some(32768),
        Replies(vec![
            tool_reply("Write", &json!({"file_path": target, "content": "x"})),
            tool_reply(
                "Bash",
                &json!({"command": format!("touch {}", marker.display())}),
            ),
            text_reply("gave up"),
        ]),
    )
    .await;

    let output = run(&server, &dir, &["--model", "m", "write it"]).await;

    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    assert!(!target.exists());
    assert!(!marker.exists());
    assert!(err(&output).contains("denied"), "{}", err(&output));
}

#[tokio::test(flavor = "multi_thread")]
async fn non_interactive_never_loads_a_model_when_auto_load_is_off() {
    let server = server(Some(1024), Replies(vec![text_reply("no")])).await;
    let args = [
        "--config-override",
        "agent.auto_load=false",
        "--model",
        "m",
        "hi",
    ];

    let output = run(&server, &workspace(), &args).await;

    assert_eq!(output.status.code(), Some(1));
    assert!(err(&output).contains("needs loading"), "{}", err(&output));
    assert!(requests_to(&server, LOAD).await.is_empty());
    assert!(requests_to(&server, CHAT).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_load_maps_to_an_allowed_load() {
    let server = server(None, Replies(vec![text_reply("loaded")])).await;
    let args = [
        "--config-override",
        "agent.auto_load=true",
        "--model",
        "m",
        "hi",
    ];

    let output = run(&server, &workspace(), &args).await;

    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    assert_eq!(requests_to(&server, LOAD).await.len(), 1);
    assert_eq!(requests_to(&server, CHAT).await[0]["model"], "m:2");
}

#[tokio::test(flavor = "multi_thread")]
async fn user_config_chooses_the_model_and_allows_loading() {
    let server = server(None, Replies(vec![text_reply("loaded")])).await;
    let dir = workspace();
    fs::create_dir_all(dir.join("xdg/praxis")).unwrap();
    let config = "default_model = \"m\"\n[agent]\nauto_load = true\n";
    fs::write(dir.join("xdg/praxis/config.toml"), config).unwrap();

    let output = run(&server, &dir, &["hi"]).await;

    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    assert_eq!(requests_to(&server, CHAT).await[0]["model"], "m:2");
}

#[tokio::test(flavor = "multi_thread")]
async fn project_config_cannot_allow_loading() {
    let server = server(Some(1024), Replies(vec![text_reply("no")])).await;
    let dir = workspace();
    fs::create_dir_all(dir.join("xdg/praxis")).unwrap();
    fs::write(
        dir.join("xdg/praxis/config.toml"),
        "[agent]\nauto_load = false\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join(".praxis")).unwrap();
    fs::write(
        dir.join(".praxis/config.toml"),
        "[agent]\nauto_load = true\n",
    )
    .unwrap();

    let output = run(&server, &dir, &["--model", "m", "hi"]).await;

    assert_eq!(output.status.code(), Some(1));
    assert!(
        err(&output).contains("Ignored `agent.auto_load`"),
        "{}",
        err(&output)
    );
    assert!(requests_to(&server, LOAD).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn models_lists_keys_and_effective_context() {
    let server = server(Some(4096), Replies(vec![])).await;

    let output = run(&server, &workspace(), &["models"]).await;

    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    let listing = out(&output);
    assert!(
        listing.contains('m') && listing.contains("4096"),
        "{listing}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_and_config_errors_exit_2() {
    let dir = workspace();
    for args in [
        vec!["--no-such-flag"],
        vec!["--config-override", "nonsense=1", "hi"],
        vec!["--config-override", "lmstudio.base_url=ftp://x", "hi"],
        vec!["--config-override", "agent.auto_load=maybe", "hi"],
    ] {
        let output = command("http://127.0.0.1:9", &dir, &args)
            .output()
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}: {}", err(&output));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_server_exits_1_with_advice() {
    let output = command("http://127.0.0.1:9", &workspace(), &["hi"])
        .output()
        .await
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        err(&output).contains("Start its local server"),
        "{}",
        err(&output)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_failure_mid_turn_exits_1() {
    let failure = "event: error\ndata: {\"error\":{\"message\":\"boom\"}}\n\n".to_owned();
    let server = server(Some(32768), Replies(vec![failure])).await;

    let output = run(&server, &workspace(), &["--model", "m", "hi"]).await;

    assert_eq!(output.status.code(), Some(1));
    assert!(err(&output).contains("boom"), "{}", err(&output));
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_c_during_a_turn_exits_130() {
    let server = server(Some(32768), Hang).await;
    let child: Child = command(&server.uri(), &workspace(), &["--model", "m", "hi"])
        .spawn()
        .unwrap();
    let chat_arrived = async {
        while requests_to(&server, CHAT).await.is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), chat_arrived)
        .await
        .unwrap();

    let pid = child.id().unwrap().to_string();
    let killed = Command::new("kill").args(["-INT", &pid]).status();
    assert!(killed.await.unwrap().success());
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(output.status.code(), Some(130), "{}", err(&output));
}

#[tokio::test(flavor = "multi_thread")]
async fn terminal_escapes_from_the_model_never_reach_the_terminal() {
    let server = server(
        Some(32768),
        Replies(vec![text_reply("a\u{1b}[2Jb\u{9b}c\nd")]),
    )
    .await;

    let output = run(&server, &workspace(), &["--model", "m", "hi"]).await;

    assert_eq!(out(&output).trim(), "a[2Jbc\nd");
}

/// `script` gives the binary a real terminal, the only way to reach the REPL and approvals.
async fn in_terminal(url: &str, dir: &Path, args: &str, typed: &str) -> Output {
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

#[tokio::test(flavor = "multi_thread")]
async fn repl_asks_to_load_then_asks_before_writing_and_keeps_context_between_prompts() {
    let dir = workspace();
    let approved = dir.join("approved.txt");
    let refused = dir.join("refused.txt");
    let server = server(
        None,
        Replies(vec![
            tool_reply("Write", &json!({"file_path": approved, "content": "x"})),
            text_reply("wrote it"),
            tool_reply("Write", &json!({"file_path": refused, "content": "x"})),
            text_reply("understood"),
        ]),
    )
    .await;

    let typed = "y\nfirst\ny\nsecond\nn\n/exit\n";
    let args = "--config-override agent.auto_load=false --model m";
    let output = in_terminal(&server.uri(), &dir, args, typed).await;

    assert_eq!(output.status.code(), Some(0), "{}", out(&output));
    assert_eq!(requests_to(&server, LOAD).await.len(), 1);
    assert!(approved.exists());
    assert!(!refused.exists());
    let chats = requests_to(&server, CHAT).await;
    assert_eq!(chats.last().unwrap()["messages"][1]["content"], "first");
    // The prompt shows the exact path, and Write may offer "always".
    assert!(out(&output).contains(approved.to_str().unwrap()));
    assert!(out(&output).contains("[a]lways"));
}

#[tokio::test(flavor = "multi_thread")]
async fn approving_project_config_at_the_terminal_is_remembered() {
    let dir = workspace();
    fs::create_dir_all(dir.join(".praxis")).unwrap();
    fs::write(
        dir.join(".praxis/config.toml"),
        "[agent]\nauto_load = true\n",
    )
    .unwrap();
    let server = server(None, Replies(vec![text_reply("one"), text_reply("two")])).await;

    let output = in_terminal(&server.uri(), &dir, "--model m hi", "y\n").await;
    assert_eq!(output.status.code(), Some(0), "{}", out(&output));
    assert!(out(&output).contains("wants to widen"), "{}", out(&output));
    assert_eq!(requests_to(&server, LOAD).await.len(), 1);

    // Stored: a later non-interactive run uses it without asking.
    let output = run(&server, &dir, &["--model", "m", "again"]).await;
    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    assert!(!err(&output).contains("Ignored"), "{}", err(&output));
}

#[tokio::test(flavor = "multi_thread")]
async fn declining_the_load_stops_without_loading() {
    let server = server(None, Replies(vec![text_reply("no")])).await;

    let output = in_terminal(
        &server.uri(),
        &workspace(),
        "--config-override agent.auto_load=false --model m hi",
        "n\n",
    )
    .await;

    assert_eq!(output.status.code(), Some(1));
    assert!(requests_to(&server, LOAD).await.is_empty());
    assert!(requests_to(&server, CHAT).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn bash_prompts_never_offer_always() {
    let dir = workspace();
    let server = server(
        Some(32768),
        Replies(vec![
            tool_reply("Bash", &json!({"command": "touch ran"})),
            text_reply("done"),
        ]),
    )
    .await;

    let output = in_terminal(&server.uri(), &dir, "--model m hi", "a\n").await;

    assert_eq!(output.status.code(), Some(0), "{}", out(&output));
    assert!(!out(&output).contains("[a]lways"), "{}", out(&output));
    // "a" isn't on offer here, so it must not count as approval.
    assert!(!dir.join("ran").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn acp_mode_writes_only_protocol_messages_to_stdout() {
    let server = server(Some(4096), Replies(vec![text_reply("hello")])).await;
    let dir = workspace();
    let mut child = command(&server.uri(), &dir, &["acp"])
        .env("RUST_LOG", "debug")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    // Each request waits for the previous answer, as an editor's would.
    let mut next = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {"protocolVersion": 1}});
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let mut seen = Vec::new();
    loop {
        if !next.is_null() {
            let line = format!("{}\n", next.take());
            stdin.write_all(line.as_bytes()).await.unwrap();
        }
        let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap_or_else(|| panic!("stdout closed after {seen:?}"));
        let message: Value = serde_json::from_str(&line).expect("stdout line is not JSON");
        assert_eq!(message["jsonrpc"], "2.0", "{line}");
        if message["id"] == 0 {
            next = json!({"jsonrpc": "2.0", "id": 1, "method": "session/new",
                "params": {"cwd": dir, "mcpServers": []}});
        }
        if message["id"] == 1 {
            next = json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt",
                "params": {"sessionId": message["result"]["sessionId"],
                    "prompt": [{"type": "text", "text": "hi"}]}});
        }
        let done = message["id"] == 2;
        seen.push(message);
        if done {
            break;
        }
    }
    assert_eq!(
        seen.last().unwrap()["result"]["stopReason"],
        "end_turn",
        "{seen:?}"
    );
    let chunk = |m: &Value| m["params"]["update"]["sessionUpdate"] == "agent_message_chunk";
    assert!(seen.iter().any(chunk), "{seen:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn acp_without_a_loaded_model_stays_up_and_reports_it_on_session_new() {
    let server = server(None, Replies(vec![text_reply("unused")])).await;
    let dir = workspace();
    let mut child = command(&server.uri(), &dir, &["acp"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let requests = [
        json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": 1}}),
        json!({"jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": dir, "mcpServers": []}}),
    ];
    let mut answers = Vec::new();
    for request in requests {
        stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .expect("stdout closed before the answer");
        answers.push(serde_json::from_str::<Value>(&line).unwrap());
    }
    assert!(answers[0]["result"].is_object(), "{answers:?}");
    assert!(
        answers[1]["error"]["data"]
            .as_str()
            .is_some_and(|data| data.contains("no model loaded")),
        "{answers:?}"
    );
    assert!(child.try_wait().unwrap().is_none(), "acp exited early");
}

/// An enabled plugin under the test's data dir, written directly so no git is involved.
fn enabled_plugin(dir: &Path) {
    let plugins = dir.join("xdg-data/praxis/plugins");
    let root = plugins.join("demo");
    for sub in ["skills/greet", "commands", "hooks"] {
        fs::create_dir_all(root.join(sub)).unwrap();
    }
    fs::write(
        root.join("skills/greet/SKILL.md"),
        "---\ndescription: greets-marker\n---\nSay hello.\n",
    )
    .unwrap();
    fs::write(root.join("commands/fix.md"), "Fix $ARGUMENTS now.\n").unwrap();
    let hooks = json!({"hooks": {"PreToolUse": [{"matcher": "Read", "hooks": [
        {"type": "command", "command": "echo blocked-by-hook >&2; exit 2"}]}]}});
    fs::write(root.join("hooks/hooks.json"), hooks.to_string()).unwrap();
    let installed = json!({"demo": {"source": "x", "commit": "0", "enabled": true}});
    fs::write(plugins.join("installed.json"), installed.to_string()).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn enabled_plugins_add_skills_commands_and_blocking_hooks() {
    let dir = workspace();
    enabled_plugin(&dir);
    fs::write(dir.join("secret.txt"), "file-content-marker").unwrap();
    let server = server(
        Some(32768),
        Replies(vec![
            tool_reply("Read", &json!({"file_path": dir.join("secret.txt")})),
            text_reply("done"),
        ]),
    )
    .await;

    let output = run(&server, &dir, &["--model", "m", "/demo:fix the bug"]).await;

    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    let requests = requests_to(&server, CHAT).await;
    let first = requests[0]["messages"].to_string();
    assert!(
        first.contains("demo:greet") && first.contains("greets-marker"),
        "{first}"
    );
    assert_eq!(requests[0]["messages"][1]["content"], "Fix the bug now.\n");
    let second = requests[1]["messages"].to_string();
    assert!(second.contains("blocked-by-hook"), "{second}");
    assert!(!second.contains("file-content-marker"), "{second}");
}

#[tokio::test(flavor = "multi_thread")]
async fn bare_resume_continues_the_newest_session_of_this_workspace_only() {
    let server = server(
        Some(32768),
        Replies(vec![text_reply("a"), text_reply("b"), text_reply("c")]),
    )
    .await;
    let root = workspace();
    let (here, there) = (root.join("here"), root.join("there"));
    fs::create_dir_all(&here).unwrap();
    fs::create_dir_all(&there).unwrap();

    let first = command(&server.uri(), &root, &["--model", "m", "first here"])
        .current_dir(&here)
        .output()
        .await
        .unwrap();
    assert_eq!(first.status.code(), Some(0), "{}", err(&first));
    let other = command(&server.uri(), &root, &["--model", "m", "first there"])
        .current_dir(&there)
        .output()
        .await
        .unwrap();
    assert_eq!(other.status.code(), Some(0), "{}", err(&other));
    let resumed = command(&server.uri(), &root, &["--model", "m", "again", "--resume"])
        .current_dir(&here)
        .output()
        .await
        .unwrap();
    assert_eq!(resumed.status.code(), Some(0), "{}", err(&resumed));

    let chats = requests_to(&server, CHAT).await;
    let sent = chats[2]["messages"].to_string();
    assert!(sent.contains("first here"), "{sent}");
    assert!(!sent.contains("first there"), "{sent}");
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_json_lists_this_workspace_without_lm_studio() {
    let dir = workspace();
    let server = server(Some(32768), Replies(vec![text_reply("ok")])).await;
    let saved = run(&server, &dir, &["--model", "m", "first prompt marker"]).await;
    assert_eq!(saved.status.code(), Some(0), "{}", err(&saved));

    // Nothing listens on port 9, so the listing must not reach LM Studio at all.
    let output = command("http://127.0.0.1:9", &dir, &["sessions", "--json"])
        .output()
        .await
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    let rows: Value = serde_json::from_str(&out(&output)).unwrap();
    assert_eq!(rows.as_array().map(Vec::len), Some(1), "{rows}");
    assert_eq!(rows[0]["title"], "first prompt marker", "{rows}");
    assert!(rows[0]["id"].is_string(), "{rows}");
    assert!(rows[0]["folder"].is_null(), "{rows}");
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_folder_moves_a_session_and_json_reports_it() {
    let dir = workspace();
    let server = server(Some(32768), Replies(vec![text_reply("ok")])).await;
    let saved = run(&server, &dir, &["--model", "m", "file me"]).await;
    assert_eq!(saved.status.code(), Some(0), "{}", err(&saved));
    let listing = command("http://127.0.0.1:9", &dir, &["sessions", "--json"])
        .output()
        .await
        .unwrap();
    let rows: Value = serde_json::from_str(&out(&listing)).unwrap();
    let id = rows[0]["id"].as_str().unwrap().to_owned();

    let moved = command(
        "http://127.0.0.1:9",
        &dir,
        &["sessions", "--folder", &id, "-Work"],
    )
    .output()
    .await
    .unwrap();

    assert_eq!(moved.status.code(), Some(0), "{}", err(&moved));
    let listing = command("http://127.0.0.1:9", &dir, &["sessions", "--json"])
        .output()
        .await
        .unwrap();
    let rows: Value = serde_json::from_str(&out(&listing)).unwrap();
    assert_eq!(rows[0]["folder"], "-Work", "{rows}");
}
