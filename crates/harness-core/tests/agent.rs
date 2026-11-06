//! Agent loop behaviour against a scripted fake server (SPEC §4, §14).

// Lets clippy treat helper functions here as test code for `allow-unwrap-in-tests`.
#![cfg(test)]

use harness_core::agent::{Agent, AgentConfig, AgentEvent, PermissionReply, ToolOutcome};
use harness_core::provider::Provider;
use harness_core::tools::ToolContext;
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate, matchers};

/// Replies with one scripted SSE body per request, repeating the last.
struct Script {
    bodies: Vec<String>,
    next: AtomicUsize,
}

impl Respond for Script {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let i = self.next.fetch_add(1, Ordering::SeqCst);
        let body = self.bodies[i.min(self.bodies.len() - 1)].clone();
        ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
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
        json!({"choices": [], "usage": {"prompt_tokens": 5000, "completion_tokens": 2}}),
    ])
}

fn tool_reply(calls: &[(&str, &str, Value)]) -> String {
    let deltas: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, args))| {
            json!({"index": index, "id": id, "function": {"name": name, "arguments": args.to_string()}})
        })
        .collect();
    sse(&[
        json!({"choices": [{"delta": {"tool_calls": deltas}, "finish_reason": null}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

fn new_workspace() -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("harness-agent-{}-{id}", std::process::id()));
    fs::create_dir_all(&path).unwrap();
    path.canonicalize().unwrap()
}

struct Harness {
    server: MockServer,
    workspace: PathBuf,
    agent: Option<Agent>,
}

impl Harness {
    async fn new(bodies: Vec<String>, max_turn_requests: u32) -> Self {
        Self::in_workspace(new_workspace(), bodies, max_turn_requests).await
    }

    async fn in_workspace(workspace: PathBuf, bodies: Vec<String>, max_turn_requests: u32) -> Self {
        let server = MockServer::start().await;
        Mock::given(matchers::method("POST"))
            .respond_with(Script {
                bodies,
                next: AtomicUsize::new(0),
            })
            .mount(&server)
            .await;
        let agent = Agent::new(
            Arc::new(Provider::new(&server.uri(), None).unwrap()),
            Arc::new(ToolContext::new(&workspace).unwrap()),
            AgentConfig {
                model: "m".into(),
                context_size: 100_000,
                system_prompt: "sys".into(),
                agents_md: None,
                max_turn_requests,
                profile: harness_core::agent::profile::ModelProfile::default(),
            },
        );
        Self {
            server,
            workspace,
            agent: Some(agent),
        }
    }

    /// Runs a turn on its own task, as a session does, and returns its event labels.
    /// `reply` answers permission requests; `None` drops the sender, as if nobody can answer.
    async fn turn(
        &mut self,
        prompt: &str,
        cancel: CancellationToken,
        mut reply: impl FnMut(&str) -> Option<PermissionReply>,
    ) -> Vec<String> {
        let mut agent = self.agent.take().unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let prompt = prompt.to_owned();
        let task = tokio::spawn(async move {
            agent.run_turn(prompt, &tx, cancel).await;
            agent
        });
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(label(&event));
            if let AgentEvent::PermissionRequest { name, respond, .. } = event
                && let Some(answer) = reply(&name)
            {
                respond.send(answer).unwrap();
            }
        }
        self.agent = Some(task.await.unwrap());
        events
    }

    async fn request_messages(&self, index: usize) -> Vec<Value> {
        let requests = self.server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[index].body).unwrap();
        body["messages"].as_array().unwrap().clone()
    }

    async fn roles(&self, index: usize) -> Vec<String> {
        self.request_messages(index)
            .await
            .iter()
            .map(|m| m["role"].as_str().unwrap().to_owned())
            .collect()
    }
}

fn label(event: &AgentEvent) -> String {
    match event {
        AgentEvent::TextDelta(text) => format!("text:{text}"),
        AgentEvent::ThinkingDelta(text) => format!("thinking:{text}"),
        AgentEvent::ToolStarted { id, title, .. } => format!("started:{id}:{title}"),
        AgentEvent::ToolFinished { id, outcome } => match outcome {
            ToolOutcome::Completed(output) if output.is_error => format!("error:{id}"),
            ToolOutcome::Completed(_) => format!("finished:{id}"),
            ToolOutcome::Cancelled => format!("cancelled:{id}"),
        },
        AgentEvent::PermissionRequest {
            id,
            can_always,
            input,
            ..
        } => format!("ask:{id}:{can_always}:{input}"),
        AgentEvent::Usage { used_tokens, .. } => format!("usage:{used_tokens}"),
        AgentEvent::Notice { .. } => "notice".to_owned(),
        AgentEvent::TurnEnded(reason) => format!("ended:{reason:?}"),
    }
}

fn never_asked(tool: &str) -> Option<PermissionReply> {
    panic!("unexpected permission request for {tool}")
}

#[tokio::test(flavor = "multi_thread")]
async fn completes_a_two_tool_turn_with_exact_event_order() {
    let workspace = new_workspace();
    let path = workspace.join("a.txt");
    fs::write(&path, "hello\n").unwrap();
    let script = vec![
        tool_reply(&[
            ("c1", "Read", json!({"file_path": path})),
            ("c2", "Glob", json!({"pattern": "*.nothing"})),
        ]),
        text_reply("done"),
    ];
    let mut h = Harness::in_workspace(workspace, script, 50).await;

    let events = h.turn("go", CancellationToken::new(), never_asked).await;

    assert_eq!(
        events,
        [
            format!("started:c1:Read {}", path.display()),
            "finished:c1".to_owned(),
            "started:c2:Glob *.nothing".to_owned(),
            "finished:c2".to_owned(),
            "text:done".to_owned(),
            "usage:5002".to_owned(),
            "ended:EndTurn".to_owned(),
        ]
    );
    assert_eq!(
        h.roles(1).await,
        ["system", "user", "assistant", "tool", "tool"]
    );
    let second = h.request_messages(1).await;
    assert!(second[3]["content"].as_str().unwrap().contains("hello"));
    assert!(h.workspace.join("a.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_mid_tool_ends_cancelled_and_the_next_prompt_has_consistent_history() {
    let mut h = Harness::new(
        vec![
            tool_reply(&[
                ("c1", "Bash", json!({"command": "sleep 30"})),
                ("c2", "Glob", json!({"pattern": "*"})),
            ]),
            text_reply("resumed"),
        ],
        50,
    )
    .await;
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();

    let started = std::time::Instant::now();
    let events = h
        .turn("first", cancel, move |_| {
            let trigger = trigger.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                trigger.cancel();
            });
            Some(PermissionReply::AllowOnce)
        })
        .await;

    assert!(started.elapsed().as_secs() < 20, "the sleep wasn't killed");
    assert_eq!(
        events,
        [
            "started:c1:Bash sleep 30",
            r#"ask:c1:false:{"command":"sleep 30"}"#,
            "cancelled:c1",
            "ended:Cancelled"
        ]
    );

    let events = h
        .turn("second", CancellationToken::new(), never_asked)
        .await;
    assert_eq!(events, ["text:resumed", "usage:5002", "ended:EndTurn"]);
    assert_eq!(
        h.roles(1).await,
        ["system", "user", "assistant", "tool", "tool", "user"]
    );
    let messages = h.request_messages(1).await;
    for (message, id) in messages[3..5].iter().zip(["c1", "c2"]) {
        assert_eq!(message["tool_call_id"], id);
        assert!(message["content"].as_str().unwrap().contains("Cancelled"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stops_at_max_turn_requests() {
    let looping = tool_reply(&[("c1", "Glob", json!({"pattern": "*"}))]);
    let mut h = Harness::new(vec![looping], 2).await;

    let events = h.turn("go", CancellationToken::new(), never_asked).await;

    assert_eq!(h.server.received_requests().await.unwrap().len(), 2);
    assert_eq!(events.last().unwrap(), "ended:MaxTurnRequests");
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_arguments_go_back_to_the_model_instead_of_ending_the_turn() {
    let bad = sse(&[
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1",
            "function": {"name": "Read", "arguments": "{not json"}}]}, "finish_reason": null}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ]);
    let mut h = Harness::new(vec![bad, text_reply("ok")], 50).await;

    let events = h.turn("go", CancellationToken::new(), never_asked).await;

    assert_eq!(
        events,
        [
            "started:c1:Read",
            "error:c1",
            "text:ok",
            "usage:5002",
            "ended:EndTurn"
        ]
    );
    let tool_result = h.request_messages(1).await[3]["content"].clone();
    assert!(tool_result.as_str().unwrap().contains("Invalid arguments"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_failure_ends_the_turn_and_the_next_prompt_still_works() {
    let error = "event: error\ndata: {\"error\":{\"message\":\"boom\"}}\n\n".to_owned();
    let mut h = Harness::new(vec![error, text_reply("fine")], 50).await;

    let events = h.turn("first", CancellationToken::new(), never_asked).await;
    assert_eq!(events, ["notice", "ended:ProviderError"]);

    let events = h
        .turn("second", CancellationToken::new(), never_asked)
        .await;
    assert_eq!(events, ["text:fine", "usage:5002", "ended:EndTurn"]);
    assert_eq!(h.roles(1).await, ["system", "user", "assistant", "user"]);
}

/// A workspace, its target file, and a script that writes the file `writes` times, then stops.
async fn writer(writes: usize) -> (Harness, PathBuf) {
    let workspace = new_workspace();
    let path = workspace.join("out.txt");
    let mut script: Vec<String> = (0..writes)
        .map(|i| {
            let id = format!("w{i}");
            tool_reply(&[(&id, "Write", json!({"file_path": path, "content": "x"}))])
        })
        .collect();
    script.push(text_reply("done"));
    (Harness::in_workspace(workspace, script, 50).await, path)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_write_does_not_happen_and_the_model_is_told() {
    let (mut h, path) = writer(1).await;

    let events = h
        .turn("go", CancellationToken::new(), |_| {
            Some(PermissionReply::RejectOnce)
        })
        .await;

    assert!(!path.exists());
    assert_eq!(events[2], "error:w0");
    let result = h.request_messages(1).await[3]["content"].clone();
    assert!(result.as_str().unwrap().contains("rejected"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_does_not_happen_when_nobody_can_answer() {
    let (mut h, path) = writer(1).await;

    let events = h.turn("go", CancellationToken::new(), |_| None).await;

    assert!(!path.exists());
    assert_eq!(events[2], "error:w0");
    assert_eq!(events.last().unwrap(), "ended:EndTurn");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_does_not_happen_when_the_front_end_is_gone() {
    let (h, path) = writer(1).await;
    let mut agent = h.agent.unwrap();
    let (tx, rx) = mpsc::channel(1);
    drop(rx);

    agent
        .run_turn("go".into(), &tx, CancellationToken::new())
        .await;

    assert!(!path.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_prompt_shows_the_full_parameters() {
    let (mut h, path) = writer(1).await;

    let events = h.turn("go", CancellationToken::new(), |_| None).await;

    // Key order depends on whether a workspace build enables serde_json's `preserve_order`.
    assert!(events[1].starts_with("ask:w0:true:"));
    assert!(events[1].contains(r#""content":"x""#));
    assert!(events[1].contains(&format!(r#""file_path":"{}""#, path.display())));
}

#[tokio::test(flavor = "multi_thread")]
async fn allow_once_writes_and_asks_again_next_time() {
    let (mut h, path) = writer(2).await;
    let mut asked = 0;

    h.turn("go", CancellationToken::new(), |_| {
        asked += 1;
        Some(PermissionReply::AllowOnce)
    })
    .await;

    assert_eq!(asked, 2);
    assert!(path.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn allow_always_stops_asking_for_that_tool() {
    let (mut h, path) = writer(2).await;
    let mut asked = 0;

    h.turn("go", CancellationToken::new(), |_| {
        asked += 1;
        Some(PermissionReply::AllowAlways)
    })
    .await;

    assert_eq!(asked, 1);
    assert!(path.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn reject_always_denies_later_calls_without_asking() {
    let (mut h, path) = writer(2).await;
    let mut asked = 0;

    h.turn("go", CancellationToken::new(), |_| {
        asked += 1;
        Some(PermissionReply::RejectAlways)
    })
    .await;

    assert_eq!(asked, 1);
    assert!(!path.exists());
}

// Bash is unsandboxed until T3.3, so a blanket grant must be impossible even from a buggy front end.
#[tokio::test(flavor = "multi_thread")]
async fn allow_always_on_bash_only_counts_once() {
    let call = tool_reply(&[("b", "Bash", json!({"command": "true"}))]);
    let mut h = Harness::new(vec![call.clone(), call, text_reply("done")], 50).await;
    let mut asked = 0;

    let events = h
        .turn("go", CancellationToken::new(), |_| {
            asked += 1;
            Some(PermissionReply::AllowAlways)
        })
        .await;

    assert_eq!(asked, 2);
    assert!(events[1].starts_with("ask:b:false:"));
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_pending_prompt_ends_the_turn_without_running_the_tool() {
    let (mut h, path) = writer(1).await;

    let events = h
        .turn("go", CancellationToken::new(), |_| {
            Some(PermissionReply::Cancelled)
        })
        .await;

    assert!(!path.exists());
    assert_eq!(events.last().unwrap(), "ended:Cancelled");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_tool_is_refused_without_a_prompt() {
    let call = tool_reply(&[("u", "Delete", json!({"file_path": "x"}))]);
    let mut h = Harness::new(vec![call, text_reply("ok")], 50).await;

    let events = h.turn("go", CancellationToken::new(), never_asked).await;

    assert_eq!(events[1], "error:u");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repeated_call_stops_the_turn_with_a_warning_and_a_new_prompt_starts_clean() {
    let looping = tool_reply(&[("c", "Glob", json!({"pattern": "*"}))]);
    let mut h = Harness::new(vec![looping], 50).await;

    let events = h.turn("go", CancellationToken::new(), never_asked).await;

    assert_eq!(h.server.received_requests().await.unwrap().len(), 3);
    assert_eq!(events[events.len() - 2], "notice");
    assert_eq!(events.last().unwrap(), "ended:LoopDetected");

    // The script keeps repeating the call, so three more requests prove the count restarted.
    let events = h.turn("again", CancellationToken::new(), never_asked).await;
    assert_eq!(h.server.received_requests().await.unwrap().len(), 6);
    assert_eq!(events.last().unwrap(), "ended:LoopDetected");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_loop_inside_one_reply_skips_the_rest_and_keeps_history_valid() {
    let call = ("c", "Glob", json!({"pattern": "*"}));
    let batch = tool_reply(&vec![call; 5]);
    let mut h = Harness::new(vec![batch, text_reply("ok")], 50).await;

    let events = h.turn("go", CancellationToken::new(), never_asked).await;
    let started = events.iter().filter(|e| e.starts_with("started:")).count();
    assert_eq!(started, 3);
    assert_eq!(events.last().unwrap(), "ended:LoopDetected");

    h.turn("next", CancellationToken::new(), never_asked).await;
    let roles = h.roles(1).await;
    let results = roles.iter().filter(|role| *role == "tool").count();
    assert_eq!(results, 5);
    assert_eq!(roles.last().unwrap(), "user");
}

#[tokio::test(flavor = "multi_thread")]
async fn repeating_a_tool_with_different_arguments_finishes_normally() {
    let mut script: Vec<String> = (0..6)
        .map(|i| tool_reply(&[("c", "Glob", json!({"pattern": format!("*.{i}")}))]))
        .collect();
    script.push(text_reply("done"));
    let mut h = Harness::new(script, 50).await;

    let events = h.turn("go", CancellationToken::new(), never_asked).await;

    assert_eq!(events.last().unwrap(), "ended:EndTurn");
}
