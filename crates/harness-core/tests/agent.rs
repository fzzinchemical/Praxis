//! Agent loop behaviour against a scripted fake server (SPEC §4, §14).

// Lets clippy treat helper functions here as test code for `allow-unwrap-in-tests`.
#![cfg(test)]

use harness_core::agent::{Agent, AgentConfig, AgentEvent, StopReason, ToolOutcome};
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
        json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 2}}),
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
                context_size: 1000,
                system_prompt: "sys".into(),
                max_turn_requests,
            },
        );
        Self {
            server,
            workspace,
            agent: Some(agent),
        }
    }

    /// Runs a turn on its own task, as a session does, handing each event to `on_event`.
    async fn turn(
        &mut self,
        prompt: &str,
        cancel: CancellationToken,
        mut on_event: impl FnMut(&mut AgentEvent),
    ) -> Vec<AgentEvent> {
        let mut agent = self.agent.take().unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let prompt = prompt.to_owned();
        let task = tokio::spawn(async move {
            agent.run_turn(prompt, &tx, cancel).await;
            agent
        });
        let mut events = Vec::new();
        while let Some(mut event) = rx.recv().await {
            on_event(&mut event);
            events.push(event);
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

fn labels(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .map(|event| match event {
            AgentEvent::TextDelta(text) => format!("text:{text}"),
            AgentEvent::ThinkingDelta(text) => format!("thinking:{text}"),
            AgentEvent::ToolStarted { id, title, .. } => format!("started:{id}:{title}"),
            AgentEvent::ToolFinished { id, outcome } => match outcome {
                ToolOutcome::Completed(output) if output.is_error => format!("error:{id}"),
                ToolOutcome::Completed(_) => format!("finished:{id}"),
                ToolOutcome::Cancelled => format!("cancelled:{id}"),
            },
            AgentEvent::Usage { used_tokens, .. } => format!("usage:{used_tokens}"),
            AgentEvent::Notice { .. } => "notice".to_owned(),
            AgentEvent::TurnEnded(reason) => format!("ended:{reason:?}"),
        })
        .collect()
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

    let events = h.turn("go", CancellationToken::new(), |_| {}).await;

    assert_eq!(
        labels(&events),
        [
            format!("started:c1:Read {}", path.display()),
            "finished:c1".to_owned(),
            "started:c2:Glob *.nothing".to_owned(),
            "finished:c2".to_owned(),
            "text:done".to_owned(),
            "usage:12".to_owned(),
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
        .turn("first", cancel, move |event| {
            if matches!(event, AgentEvent::ToolStarted { .. }) {
                let trigger = trigger.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    trigger.cancel();
                });
            }
        })
        .await;

    assert!(started.elapsed().as_secs() < 20, "the sleep wasn't killed");
    assert_eq!(
        labels(&events),
        [
            "started:c1:Bash sleep 30",
            "cancelled:c1",
            "ended:Cancelled"
        ]
    );

    let events = h.turn("second", CancellationToken::new(), |_| {}).await;
    assert_eq!(
        labels(&events),
        ["text:resumed", "usage:12", "ended:EndTurn"]
    );
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

    let events = h.turn("go", CancellationToken::new(), |_| {}).await;

    assert_eq!(h.server.received_requests().await.unwrap().len(), 2);
    assert!(matches!(
        events.last(),
        Some(AgentEvent::TurnEnded(StopReason::MaxTurnRequests))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_arguments_go_back_to_the_model_instead_of_ending_the_turn() {
    let bad = sse(&[
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1",
            "function": {"name": "Read", "arguments": "{not json"}}]}, "finish_reason": null}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ]);
    let mut h = Harness::new(vec![bad, text_reply("ok")], 50).await;

    let events = h.turn("go", CancellationToken::new(), |_| {}).await;

    assert_eq!(
        labels(&events),
        [
            "started:c1:Read",
            "error:c1",
            "text:ok",
            "usage:12",
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

    let events = h.turn("first", CancellationToken::new(), |_| {}).await;
    assert_eq!(labels(&events), ["notice", "ended:ProviderError"]);

    let events = h.turn("second", CancellationToken::new(), |_| {}).await;
    assert_eq!(labels(&events), ["text:fine", "usage:12", "ended:EndTurn"]);
    assert_eq!(h.roles(1).await, ["system", "user", "assistant", "user"]);
}
