//! Runs the real binary against a fake LM Studio (SPEC §11, §14).

// Lets clippy treat helper functions here as test code for `allow-unwrap-in-tests`.
#![cfg(test)]

mod common;

use common::*;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
#[tokio::test(flavor = "multi_thread")]
async fn the_workspace_agents_md_reaches_the_system_prompt() {
    const MARKER: &str = "MARKER-7f3a use tabs";

    let with = workspace();
    fs::write(with.join("AGENTS.md"), format!("# Rules\n{MARKER}\n")).unwrap();
    let first = server(Some(32768), Replies(vec![text_reply("ok")])).await;
    let output = run(&first, &with, &["--model", "m", "say hi"]).await;
    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    let chats = requests_to(&first, CHAT).await;
    let system = &chats[0]["messages"][0];
    assert_eq!(system["role"], "system");
    assert!(
        system.to_string().contains(MARKER),
        "marker missing: {}",
        chats[0]
    );

    let without = workspace();
    let fake = server(Some(32768), Replies(vec![text_reply("ok")])).await;
    let output = run(&fake, &without, &["--model", "m", "say hi"]).await;
    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    let chats = requests_to(&fake, CHAT).await;
    assert!(
        !chats[0].to_string().contains(MARKER),
        "marker leaked without AGENTS.md"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_agents_md_is_labelled_only_while_its_approved_text_matches() {
    const RULES: &str = "Use tabs.";
    let dir = workspace();
    let path = dir.join("AGENTS.md");
    fs::write(&path, RULES).unwrap();
    let label = format!("Instructions from: {}", path.display());
    let system_of = |chats: &[Value]| {
        chats[0]["messages"][0]["content"]
            .as_str()
            .unwrap()
            .to_owned()
    };

    // Piped input cannot approve, so the file loads as plain context.
    let unapproved = server(Some(32768), Replies(vec![text_reply("ok")])).await;
    let output = run(&unapproved, &dir, &["--model", "m", "say hi"]).await;
    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    let system = system_of(&requests_to(&unapproved, CHAT).await);
    assert!(system.contains(RULES), "{system}");
    assert!(!system.contains(&label), "{system}");

    // A stored approval of exactly this text labels it.
    let store = dir.join("xdg-data/praxis/trust.json");
    fs::create_dir_all(store.parent().unwrap()).unwrap();
    let approvals = json!({ path.display().to_string(): praxis_core::trust::hash(RULES) });
    fs::write(&store, approvals.to_string()).unwrap();
    let approved = server(Some(32768), Replies(vec![text_reply("ok")])).await;
    let output = run(&approved, &dir, &["--model", "m", "say hi"]).await;
    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    let system = system_of(&requests_to(&approved, CHAT).await);
    assert!(system.contains(&format!("{label}\n\n{RULES}")), "{system}");

    // Any change to the text drops back to plain context until approved again.
    fs::write(&path, "Use spaces.").unwrap();
    let changed = server(Some(32768), Replies(vec![text_reply("ok")])).await;
    let output = run(&changed, &dir, &["--model", "m", "say hi"]).await;
    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    let system = system_of(&requests_to(&changed, CHAT).await);
    assert!(system.contains("Use spaces."), "{system}");
    assert!(!system.contains(&label), "{system}");
    fs::remove_dir_all(dir).unwrap();
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
async fn without_a_model_flag_the_cli_adds_no_model_listing_of_its_own() {
    let server = server(Some(4096), Replies(vec![text_reply("ok")])).await;

    let output = run(&server, &workspace(), &["hi"]).await;

    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    let listings = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/api/v1/models")
        .count();
    // One lists the models to pick the loaded one; the other is `ensure_ready`'s own check
    // in praxis-core. The CLI adds no third listing.
    // shortcut: ensure_ready lists again on every call; drop to 1 once core takes the list.
    assert_eq!(listings, 2);
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
async fn models_marks_an_unloaded_model_with_its_maximum_context() {
    let server = server(None, Replies(vec![])).await;

    let output = run(&server, &workspace(), &["models"]).await;

    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    assert!(
        out(&output).contains("not loaded, up to 99999"),
        "{}",
        out(&output)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_without_an_id_needs_an_earlier_session_in_this_workspace() {
    let dir = workspace();
    let server = server(Some(32768), Replies(vec![text_reply("ok")])).await;

    let output = run(&server, &dir, &["--model", "m", "say hi", "--resume"]).await;

    assert_eq!(output.status.code(), Some(1), "{}", out(&output));
    assert!(
        err(&output).contains("no earlier session in this workspace"),
        "{}",
        err(&output)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_with_an_unknown_id_names_the_session_it_could_not_resume() {
    let dir = workspace();
    let server = server(Some(32768), Replies(vec![text_reply("ok")])).await;

    let output = run(&server, &dir, &["--model", "m", "say hi", "--resume=nope"]).await;

    assert_eq!(output.status.code(), Some(1), "{}", out(&output));
    assert!(
        err(&output).contains("can't resume session `nope`"),
        "{}",
        err(&output)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sandbox_switched_off_in_config_says_so_before_the_run() {
    let dir = workspace();
    let server = server(Some(32768), Replies(vec![text_reply("ok")])).await;

    let output = run(
        &server,
        &dir,
        &[
            "--config-override",
            "sandbox.mode=off",
            "--model",
            "m",
            "say hi",
        ],
    )
    .await;

    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    assert!(
        err(&output).contains("The sandbox is off"),
        "{}",
        err(&output)
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
/// Sends one request and returns its answer, skipping the notifications that come before it.
async fn ask(
    stdin: &mut tokio::process::ChildStdin,
    lines: &mut tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
    id: u64,
    method: &str,
    params: Value,
) -> Value {
    let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    stdin
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    loop {
        let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .expect("stdout closed");
        let message: Value = serde_json::from_str(&line).expect("stdout line is not JSON");
        if message["id"] == id {
            return message;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn acp_lists_a_waiting_project_server_and_approval_stores_and_connects_it() {
    let server = server(Some(4096), Replies(vec![text_reply("unused")])).await;
    let dir = workspace();
    fs::write(
        dir.join(".mcp.json"),
        r#"{"mcpServers": {"fs": {"command": "/nonexistent/mcp-fs", "args": ["."]}}}"#,
    )
    .unwrap();
    let mut child = command(&server.uri(), &dir, &["acp"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    ask(
        &mut stdin,
        &mut lines,
        0,
        "initialize",
        json!({"protocolVersion": 1}),
    )
    .await;
    let created = ask(
        &mut stdin,
        &mut lines,
        1,
        "session/new",
        json!({"cwd": dir, "mcpServers": []}),
    )
    .await;
    let session = created["result"]["sessionId"].clone();

    let listed = ask(
        &mut stdin,
        &mut lines,
        2,
        "_praxis/mcp_servers",
        json!({"sessionId": session}),
    )
    .await;
    let fs_row = &listed["result"]["servers"][0];
    assert_eq!(fs_row["name"], "fs", "{listed}");
    assert_eq!(fs_row["status"], "needs_approval", "{listed}");
    assert_eq!(fs_row["command"], "/nonexistent/mcp-fs .", "{listed}");

    // The server can't start, so the approval is stored and the answer says why it isn't connected.
    let approved = ask(
        &mut stdin,
        &mut lines,
        3,
        "_praxis/approve_mcp_server",
        json!({"sessionId": session, "name": "fs"}),
    )
    .await;
    assert_eq!(approved["result"]["name"], "fs", "{approved}");
    assert!(
        approved["result"]["notices"][0]
            .as_str()
            .unwrap()
            .contains("fs"),
        "{approved}"
    );
    let stored = fs::read_to_string(dir.join("xdg-data/praxis/trust.json")).unwrap();
    assert!(
        stored.contains("server:") && stored.contains("#fs"),
        "{stored}"
    );
    let relisted = ask(
        &mut stdin,
        &mut lines,
        4,
        "_praxis/mcp_servers",
        json!({"sessionId": session}),
    )
    .await;
    assert_eq!(
        relisted["result"]["servers"][0]["status"], "failed",
        "{relisted}"
    );

    let unknown = ask(
        &mut stdin,
        &mut lines,
        5,
        "_praxis/approve_mcp_server",
        json!({"sessionId": session, "name": "nope"}),
    )
    .await;
    assert!(unknown["error"].is_object(), "{unknown}");
    let no_name = ask(
        &mut stdin,
        &mut lines,
        6,
        "_praxis/approve_mcp_server",
        json!({"sessionId": session}),
    )
    .await;
    assert!(no_name["error"].is_object(), "{no_name}");
    let _ = child.kill().await;
}

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
async fn rewind_restores_the_file_and_the_chat_to_before_the_turn() {
    let dir = workspace();
    let kept = dir.join("kept.txt");
    let undone = dir.join("undone.txt");
    let server = server(
        None,
        Replies(vec![
            tool_reply("Write", &json!({"file_path": kept, "content": "x"})),
            text_reply("wrote kept"),
            // Each turn wrote a file, so T13.7's check note asks once more.
            text_reply("no checks here"),
            tool_reply("Write", &json!({"file_path": undone, "content": "x"})),
            text_reply("wrote undone"),
            text_reply("no checks here"),
            text_reply("fresh start"),
        ]),
    )
    .await;

    let typed = "y\nfirst\ny\nsecond\ny\n/rewind\n/rewind 2\nthird\n/exit\n";
    let args = "--config-override agent.auto_load=false --model m";
    let output = in_terminal(&server.uri(), &dir, args, typed).await;

    assert_eq!(output.status.code(), Some(0), "{}", out(&output));
    assert!(out(&output).contains("2: second"), "{}", out(&output));
    assert!(kept.exists());
    assert!(!undone.exists());
    let chats = requests_to(&server, CHAT).await;
    let sent = chats.last().unwrap()["messages"].to_string();
    assert!(sent.contains("third") && !sent.contains("second"), "{sent}");
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

#[tokio::test(flavor = "multi_thread")]
async fn profile_needs_a_model_and_a_valid_field() {
    let dir = workspace();
    let no_model = command("http://127.0.0.1:9", &dir, &["profile", "show"])
        .output()
        .await
        .unwrap();
    assert_eq!(no_model.status.code(), Some(1), "{}", err(&no_model));
    assert!(
        err(&no_model).contains("default_model"),
        "{}",
        err(&no_model)
    );

    let unknown = command(
        "http://127.0.0.1:9",
        &dir,
        &["profile", "set", "contxt", "1", "--model", "m"],
    )
    .output()
    .await
    .unwrap();
    assert_eq!(unknown.status.code(), Some(2), "{}", err(&unknown));
    assert!(!dir.join("xdg/praxis/config.toml").exists());

    // Nothing listens on port 9: show still prints, with a note that capabilities are unknown.
    let offline = command("http://127.0.0.1:9", &dir, &["profile", "show", "m"])
        .output()
        .await
        .unwrap();
    assert_eq!(offline.status.code(), Some(0), "{}", err(&offline));
    assert!(err(&offline).contains("unreachable"), "{}", err(&offline));
    assert!(
        out(&offline).contains("max_output = unset  (default)"),
        "{}",
        out(&offline)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_list_prints_each_servers_name_and_status() {
    let dir = workspace();
    fs::create_dir_all(dir.join("xdg/praxis")).unwrap();
    fs::write(
        dir.join("xdg/praxis/config.toml"),
        "[mcp.docs]\ncommand = \"/nonexistent/mcp-docs\"\n",
    )
    .unwrap();
    fs::write(
        dir.join(".mcp.json"),
        r#"{"mcpServers": {"fs": {"command": "/nonexistent/mcp-fs"}}}"#,
    )
    .unwrap();

    let output = command("http://127.0.0.1:9", &dir, &["mcp", "list"])
        .output()
        .await
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    let listing = out(&output);
    assert!(listing.contains("docs (configured)"), "{listing}");
    assert!(listing.contains("fs (needs_approval)"), "{listing}");
}

#[tokio::test(flavor = "multi_thread")]
async fn profile_set_writes_under_home_config_when_xdg_config_home_is_unset() {
    let dir = workspace();
    let output = command(
        "http://127.0.0.1:9",
        &dir,
        &["profile", "set", "temperature", "0.3", "--model", "m"],
    )
    .env_remove("XDG_CONFIG_HOME")
    .output()
    .await
    .unwrap();

    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    assert!(dir.join(".config/praxis/config.toml").is_file());
}

#[tokio::test(flavor = "multi_thread")]
async fn profile_set_refuses_without_a_model_or_a_home_to_write_to() {
    let dir = workspace();
    let no_model = command(
        "http://127.0.0.1:9",
        &dir,
        &["profile", "set", "temperature", "0.3"],
    )
    .output()
    .await
    .unwrap();
    assert_eq!(no_model.status.code(), Some(1), "{}", err(&no_model));
    assert!(
        err(&no_model).contains("pass a model"),
        "{}",
        err(&no_model)
    );
    assert!(!dir.join("xdg/praxis/config.toml").exists());

    let no_home = command(
        "http://127.0.0.1:9",
        &dir,
        &["profile", "set", "temperature", "0.3", "--model", "m"],
    )
    .env_remove("HOME")
    .env_remove("XDG_CONFIG_HOME")
    .output()
    .await
    .unwrap();
    assert_eq!(no_home.status.code(), Some(2), "{}", err(&no_home));
    assert!(err(&no_home).contains("needs HOME"), "{}", err(&no_home));
}

#[tokio::test(flavor = "multi_thread")]
async fn profile_show_json_lists_each_field_with_its_value_and_source() {
    let dir = workspace();
    let server = server(Some(4096), Replies(vec![])).await;
    let shown = run(&server, &dir, &["profile", "show", "--json", "m"]).await;

    assert_eq!(shown.status.code(), Some(0), "{}", err(&shown));
    let rows: Value = serde_json::from_str(&out(&shown)).unwrap();
    let rows = rows.as_array().unwrap();
    let field = |name: &str| rows.iter().find(|r| r["field"] == name).unwrap();
    assert_eq!(field("max_output")["value"], Value::Null);
    assert_eq!(field("max_output")["source"], "default");
    assert_eq!(field("context")["value"], Value::Null);
    assert_eq!(field("temperature")["source"], "default");
}

#[tokio::test(flavor = "multi_thread")]
async fn profile_set_writes_the_user_layer_that_profile_show_reports() {
    let dir = workspace();
    let set = command(
        "http://127.0.0.1:9",
        &dir,
        &["profile", "set", "temperature", "0.3", "--model", "m"],
    )
    .output()
    .await
    .unwrap();
    assert_eq!(set.status.code(), Some(0), "{}", err(&set));

    let server = server(Some(4096), Replies(vec![])).await;
    let shown = run(&server, &dir, &["profile", "show", "m"]).await;

    assert_eq!(shown.status.code(), Some(0), "{}", err(&shown));
    assert!(
        out(&shown)
            .lines()
            .any(|line| line == "temperature = 0.3  (user \"m\")"),
        "{}",
        out(&shown)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn max_tokens_is_capped_by_the_profiles_max_output() {
    let dir = workspace();
    fs::create_dir_all(dir.join("xdg/praxis")).unwrap();
    fs::write(
        dir.join("xdg/praxis/config.toml"),
        "[profiles.m]\nmax_output = 8192\n",
    )
    .unwrap();
    let server = server(Some(32768), Replies(vec![text_reply("ok")])).await;

    let output = run(&server, &dir, &["--model", "m", "hi"]).await;

    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    assert_eq!(requests_to(&server, CHAT).await[0]["max_tokens"], 8192);
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_list_json_names_each_configured_source_without_connecting() {
    let dir = workspace();
    fs::create_dir_all(dir.join("xdg/praxis")).unwrap();
    fs::write(
        dir.join("xdg/praxis/config.toml"),
        "[mcp.docs]\ncommand = \"/nonexistent/mcp-docs\"\n",
    )
    .unwrap();
    fs::write(
        dir.join(".mcp.json"),
        r#"{"mcpServers": {"fs": {"command": "/nonexistent/mcp-fs", "args": ["."]}}}"#,
    )
    .unwrap();

    let output = command("http://127.0.0.1:9", &dir, &["mcp", "list", "--json"])
        .output()
        .await
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    let listing: Value = serde_json::from_str(&out(&output)).unwrap();
    let servers = listing["servers"].as_array().expect("servers array");
    let docs = servers
        .iter()
        .find(|s| s["name"] == "docs")
        .expect("user server");
    assert_eq!(docs["source"], "user", "{listing}");
    assert_eq!(docs["status"], "configured", "{listing}");
    let fs_row = servers
        .iter()
        .find(|s| s["name"] == "fs")
        .expect("project server");
    assert_eq!(fs_row["source"], "project", "{listing}");
    assert_eq!(fs_row["status"], "needs_approval", "{listing}");
    assert_eq!(fs_row["command"], "/nonexistent/mcp-fs", "{listing}");
}

#[tokio::test(flavor = "multi_thread")]
async fn acp_rejects_praxis_requests_missing_their_params_and_keeps_serving() {
    let server = server(Some(32768), Replies(vec![text_reply("hello")])).await;
    let dir = workspace();
    let mut child = command(&server.uri(), &dir, &["acp"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    assert!(
        ask(
            &mut stdin,
            &mut lines,
            0,
            "initialize",
            json!({"protocolVersion": 1})
        )
        .await["result"]
            .is_object()
    );
    let session = ask(
        &mut stdin,
        &mut lines,
        1,
        "session/new",
        json!({"cwd": dir, "mcpServers": []}),
    )
    .await;
    let session_id = session["result"]["sessionId"].clone();

    // A notification gets no answer, so it only has to leave the connection usable.
    let stop = json!({"jsonrpc": "2.0", "method": "_praxis/cancel_subagent",
        "params": {"sessionId": session_id, "toolCallId": "none"}});
    stdin
        .write_all(format!("{stop}\n").as_bytes())
        .await
        .unwrap();

    for (id, method) in [
        (2, "_praxis/revert_file"),
        (3, "_praxis/set_mcp_enabled"),
        (4, "_praxis/rewind"),
        (5, "_praxis/approve_mcp_server"),
    ] {
        let answer = ask(&mut stdin, &mut lines, id, method, json!({})).await;
        assert_eq!(answer["error"]["code"], -32602, "{method}: {answer}");
    }

    let prompt = json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "hi"}]});
    assert_eq!(
        ask(&mut stdin, &mut lines, 6, "session/prompt", prompt).await["result"]["stopReason"],
        "end_turn"
    );
    // Closing stdin lets the agent exit on its own, so its coverage profile is written.
    drop(stdin);
    assert!(child.wait().await.unwrap().success());
}

#[tokio::test(flavor = "multi_thread")]
async fn acp_sessions_load_the_users_claude_md() {
    const MARKER: &str = "MARKER-acp-claude-md";
    let server = server(Some(32768), Replies(vec![text_reply("hello")])).await;
    let dir = workspace();
    let home = dir.join("home");
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::write(home.join(".claude/CLAUDE.md"), MARKER).unwrap();
    let mut child = command(&server.uri(), &dir, &["acp"])
        .env("HOME", &home)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let init = json!({"protocolVersion": 1});
    assert!(ask(&mut stdin, &mut lines, 0, "initialize", init).await["result"].is_object());
    let new = json!({"cwd": dir, "mcpServers": []});
    let session = ask(&mut stdin, &mut lines, 1, "session/new", new).await;
    let prompt = json!({"sessionId": session["result"]["sessionId"],
        "prompt": [{"type": "text", "text": "hi"}]});
    assert_eq!(
        ask(&mut stdin, &mut lines, 2, "session/prompt", prompt).await["result"]["stopReason"],
        "end_turn"
    );
    let chats = requests_to(&server, CHAT).await;
    assert!(chats[0].to_string().contains(MARKER), "{}", chats[0]);
}
