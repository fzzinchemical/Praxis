//! Agent loop behaviour against a scripted fake server (SPEC §4, §14).

// Lets clippy treat helper functions here as test code for `allow-unwrap-in-tests`.
#![cfg(test)]

use harness_core::agent::{Agent, AgentConfig, AgentEvent, PermissionReply, ToolOutcome};
use harness_core::permissions::Policy;
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
        Self::with_policy(workspace, bodies, max_turn_requests, Policy::default()).await
    }

    async fn with_policy(
        workspace: PathBuf,
        bodies: Vec<String>,
        max_turn_requests: u32,
        policy: Policy,
    ) -> Self {
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
                memory: Vec::new(),
                scoped_rules: Vec::new(),
                mask_keep_turns: 10,
                max_turn_requests,
                profile: harness_core::agent::profile::ModelProfile::default(),
                seed: None,
                policy,
                masking: true,
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
        AgentEvent::Compaction(state) => format!("compaction:{state:?}"),
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
async fn a_delegated_agent_runs_with_its_own_history_and_tools_and_returns_a_summary() {
    let script = vec![
        tool_reply(&[(
            "p1",
            "Agent",
            json!({"subagent_type": "kit:scout", "prompt": "find it"}),
        )]),
        // The subagent tries a tool it isn't allowed, then answers.
        tool_reply(&[("s1", "Write", json!({"file_path": "x", "content": "y"}))]),
        text_reply("found it in a.rs"),
        text_reply("done"),
    ];
    let mut h = Harness::new(script, 50).await;
    let scout = harness_core::plugin::AgentDef {
        skill: harness_core::plugin::Skill {
            name: "kit:scout".into(),
            description: "Finds things".into(),
            body: "You scout.".into(),
            model_invocable: true,
            user_invocable: true,
            dir: Default::default(),
        },
        tools: Some(vec!["Read".into(), "Write".into()]),
        disallowed_tools: vec!["Write".into()],
        max_turns: Some(5),
        model: None,
    };
    h.agent = Some(h.agent.take().unwrap().with_agents(vec![scout]));

    let events = h.turn("go", CancellationToken::new(), never_asked).await;

    assert!(events.contains(&"error:s1".to_owned()), "{events:?}");
    assert!(!h.workspace.join("x").exists());
    // The subagent starts from its own prompt, without the parent's conversation.
    let child = h.request_messages(1).await;
    assert_eq!(child.len(), 2);
    assert!(child[0]["content"].as_str().unwrap().contains("You scout."));
    assert_eq!(child[1]["content"], "find it");
    let requests = h.server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
    let tools: Vec<&str> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(tools, ["Read"]);
    let parent = h.request_messages(3).await;
    assert_eq!(parent.len(), 4);
    assert_eq!(parent[3]["content"], "found it in a.rs");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delegated_agent_with_a_model_runs_on_it_and_the_parent_keeps_its_own() {
    let script = vec![
        tool_reply(&[(
            "p1",
            "Agent",
            json!({"subagent_type": "kit:scout", "prompt": "find it"}),
        )]),
        text_reply("found it in a.rs"),
        text_reply("done"),
    ];
    let mut h = Harness::new(script, 50).await;
    Mock::given(matchers::method("GET"))
        .and(matchers::path("/api/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": [{
            "key": "worker",
            "loaded_instances": [{"id": "worker-1", "config": {"context_length": 100_000}}]
        }]})))
        .mount(&h.server)
        .await;
    let scout = harness_core::plugin::AgentDef {
        skill: harness_core::plugin::Skill {
            name: "kit:scout".into(),
            description: "Finds things".into(),
            body: "You scout.".into(),
            model_invocable: true,
            user_invocable: true,
            dir: Default::default(),
        },
        tools: Some(vec!["Read".into()]),
        disallowed_tools: vec![],
        max_turns: Some(5),
        model: Some(harness_core::config::ModelRef {
            backend: harness_core::config::Backend::LmStudio,
            model: "worker".into(),
        }),
    };
    h.agent = Some(h.agent.take().unwrap().with_agents(vec![scout]));

    h.turn("go", CancellationToken::new(), never_asked).await;

    let requests = h.server.received_requests().await.unwrap();
    let models: Vec<Value> = requests
        .iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["model"].clone())
        .collect();
    assert_eq!(models, ["m", "worker-1", "m"]);
    // Index 1 is the model list; the parent resumes at index 3 with the subagent's summary.
    let parent = h.request_messages(3).await;
    assert_eq!(parent.len(), 4);
    assert_eq!(parent[3]["content"], "found it in a.rs");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delegated_agent_on_anthropic_fails_before_sending_a_request() {
    let script = vec![
        tool_reply(&[(
            "p1",
            "Agent",
            json!({"subagent_type": "kit:scout", "prompt": "find it"}),
        )]),
        text_reply("done"),
    ];
    let mut h = Harness::new(script, 50).await;
    let scout = harness_core::plugin::AgentDef {
        skill: harness_core::plugin::Skill {
            name: "kit:scout".into(),
            description: "Finds things".into(),
            body: "You scout.".into(),
            model_invocable: true,
            user_invocable: true,
            dir: Default::default(),
        },
        tools: None,
        disallowed_tools: vec![],
        max_turns: None,
        model: Some(harness_core::config::ModelRef {
            backend: harness_core::config::Backend::Anthropic,
            model: "claude-sonnet-5-5".into(),
        }),
    };
    h.agent = Some(h.agent.take().unwrap().with_agents(vec![scout]));

    let events = h.turn("go", CancellationToken::new(), never_asked).await;

    assert!(events.contains(&"error:p1".to_owned()), "{events:?}");
    let requests = h.server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "only the parent's two requests");
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

#[tokio::test(flavor = "multi_thread")]
async fn resuming_a_session_sends_the_same_history() {
    use harness_core::session::SessionLog;
    let workspace = new_workspace();
    let path = workspace.join("a.txt");
    fs::write(&path, "hello\n").unwrap();
    let dir = new_workspace().join("sessions");
    let script = vec![
        tool_reply(&[("c1", "Read", json!({"file_path": path}))]),
        text_reply("done"),
    ];
    let mut first = Harness::in_workspace(workspace.clone(), script, 50).await;
    let log = SessionLog::create(&dir, &workspace).unwrap();
    let id = log.id().to_owned();
    first.agent = Some(first.agent.take().unwrap().with_session(log, Vec::new()));
    first
        .turn("go", CancellationToken::new(), never_asked)
        .await;

    let mut second = Harness::in_workspace(workspace, vec![text_reply("again")], 50).await;
    let (log, prior) = SessionLog::resume(&dir, &id).unwrap();
    second.agent = Some(second.agent.take().unwrap().with_session(log, prior));
    second
        .turn("more", CancellationToken::new(), never_asked)
        .await;

    let mut expected = first.request_messages(1).await;
    expected.push(json!({"role": "assistant", "content": "done"}));
    expected.push(json!({"role": "user", "content": "more"}));
    assert_eq!(second.request_messages(0).await, expected);
    let (_, all) = SessionLog::resume(&dir, &id).unwrap();
    assert_eq!(all.len(), 6, "both turns are logged");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_log_write_stops_the_turn_before_any_request() {
    use harness_core::session::SessionLog;
    let mut h = Harness::new(vec![text_reply("never")], 50).await;
    let full = fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();
    let log = SessionLog::from_file("full".into(), full);
    h.agent = Some(h.agent.take().unwrap().with_session(log, Vec::new()));

    let events = h.turn("go", CancellationToken::new(), never_asked).await;

    assert_eq!(events, ["notice", "ended:LogFailed"]);
    assert!(h.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn resuming_a_log_with_unanswered_tool_calls_answers_them_before_the_next_request() {
    use harness_core::provider::{Message, ToolCall};
    use harness_core::session::SessionLog;
    let workspace = new_workspace();
    let dir = new_workspace().join("sessions");
    let call = |id: &str| ToolCall {
        id: id.into(),
        name: "Glob".into(),
        arguments: "{\"pattern\":\"*\"}".into(),
    };
    let mut log = SessionLog::create(&dir, &workspace).unwrap();
    let id = log.id().to_owned();
    log.append(&Message::User {
        content: "go".into(),
    })
    .unwrap();
    log.append(&Message::Assistant {
        content: None,
        tool_calls: vec![call("c1"), call("c2"), call("c3")],
    })
    .unwrap();
    log.append(&Message::Tool {
        tool_call_id: "c1".into(),
        content: "done".into(),
    })
    .unwrap();

    let mut h = Harness::in_workspace(workspace.clone(), vec![text_reply("ok")], 50).await;
    let (log, prior) = SessionLog::resume(&dir, &id).unwrap();
    h.agent = Some(h.agent.take().unwrap().with_session(log, prior));
    h.turn("more", CancellationToken::new(), never_asked).await;

    let messages = h.request_messages(0).await;
    let answered: Vec<&str> = messages
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["tool_call_id"].as_str().unwrap())
        .collect();
    assert_eq!(answered, ["c1", "c2", "c3"]);
    assert!(
        messages[4]["content"]
            .as_str()
            .unwrap()
            .contains("ended before")
    );

    let (_, all) = SessionLog::resume(&dir, &id).unwrap();
    assert_eq!(all.len(), 7, "the repaired results are logged too");
}

/// SPEC §11: the prompt sits in the prefix, so it stays short and covers each contract point.
#[test]
fn system_prompt_covers_the_contract_within_budget() {
    let prompt = harness_core::agent::system_prompt(std::path::Path::new("/w")).to_lowercase();
    for point in ["assumptions", "unsure", "disagree", "small", "verified"] {
        assert!(prompt.contains(point), "missing {point:?}: {prompt}");
    }
    // About 150 tokens at the §7 estimate of chars / 3.5; growth has to argue for itself.
    assert!(prompt.len() <= 520, "{} chars", prompt.len());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_without_plugins_offers_the_general_agent() {
    let mut h = Harness::new(vec![text_reply("hi")], 50).await;
    h.agent = Some(h.agent.take().unwrap().with_agents(Vec::new()));

    h.turn("go", CancellationToken::new(), never_asked).await;

    let requests = h.server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let agent = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["function"]["name"] == "Agent")
        .expect("the Agent tool is offered");
    assert!(
        agent["function"]["description"]
            .as_str()
            .unwrap()
            .contains("- general:")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_general_agent_runs_fresh_without_the_agent_tool_and_returns_its_reply() {
    let script = vec![
        tool_reply(&[(
            "p1",
            "Agent",
            json!({"subagent_type": "general", "prompt": "look"}),
        )]),
        text_reply("found it"),
        text_reply("done"),
    ];
    let mut h = Harness::new(script, 50).await;
    h.agent = Some(h.agent.take().unwrap().with_agents(Vec::new()));

    h.turn("go", CancellationToken::new(), never_asked).await;

    let child = h.request_messages(1).await;
    assert_eq!(child.len(), 2);
    assert_eq!(child[1]["content"], "look");
    let requests = h.server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
    let tools: Vec<&str> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert!(tools.contains(&"Read"), "{tools:?}");
    assert!(!tools.contains(&"Agent"), "{tools:?}");
    let parent = h.request_messages(2).await;
    assert_eq!(parent.last().unwrap()["content"], "found it");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_general_agent_obeys_session_deny_rules() {
    let script = vec![
        tool_reply(&[(
            "p1",
            "Agent",
            json!({"subagent_type": "general", "prompt": "write"}),
        )]),
        tool_reply(&[("s1", "Write", json!({"file_path": "x", "content": "y"}))]),
        text_reply("refused"),
        text_reply("done"),
    ];
    let workspace = new_workspace();
    let policy = harness_core::permissions::Policy::new(
        harness_core::permissions::Mode::Ask,
        &[],
        &["Write".to_owned()],
    )
    .unwrap();
    let mut h = Harness::with_policy(workspace.clone(), script, 50, policy).await;
    h.agent = Some(h.agent.take().unwrap().with_agents(Vec::new()));

    let events = h
        .turn("go", CancellationToken::new(), |_| {
            panic!("a denied call must not ask")
        })
        .await;

    assert!(events.contains(&"error:s1".to_owned()), "{events:?}");
    assert!(!workspace.join("x").exists());
}

/// Serves the LM Studio model list that a worker on `worker` needs to load.
async fn serve_worker_model(server: &MockServer) {
    Mock::given(matchers::method("GET"))
        .and(matchers::path("/api/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": [{
            "key": "worker",
            "loaded_instances": [{"id": "worker-1", "config": {"context_length": 100_000}}]
        }]})))
        .mount(server)
        .await;
}

fn lmstudio_worker(name: &str, disallowed: Vec<String>) -> harness_core::plugin::AgentDef {
    harness_core::plugin::AgentDef {
        skill: harness_core::plugin::Skill {
            name: name.into(),
            description: "Does one step".into(),
            body: "You do one step.".into(),
            model_invocable: true,
            user_invocable: true,
            dir: Default::default(),
        },
        tools: None,
        disallowed_tools: disallowed,
        max_turns: Some(5),
        model: Some(harness_core::config::ModelRef {
            backend: harness_core::config::Backend::LmStudio,
            model: "worker".into(),
        }),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_orchestrator_delegates_two_steps_and_keeps_only_the_reports() {
    let script = vec![
        tool_reply(&[(
            "p1",
            "Agent",
            json!({"subagent_type": "kit:step", "prompt": "step one"}),
        )]),
        text_reply("report one"),
        tool_reply(&[(
            "p2",
            "Agent",
            json!({"subagent_type": "kit:step", "prompt": "step two"}),
        )]),
        text_reply("report two"),
        text_reply("both steps are done"),
    ];
    let mut h = Harness::new(script, 50).await;
    serve_worker_model(&h.server).await;
    let step = lmstudio_worker("kit:step", vec![]);
    h.agent = Some(h.agent.take().unwrap().with_agents(vec![step]));

    h.turn("go", CancellationToken::new(), never_asked).await;

    let requests = h.server.received_requests().await.unwrap();
    let models: Vec<Value> = requests
        .iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["model"].clone())
        .collect();
    assert_eq!(models, ["m", "worker-1", "m", "worker-1", "m"]);
    // Only the two reports come back; neither worker's transcript reaches the orchestrator.
    // Requests: parent, model list, worker, parent, model list, worker, parent.
    let parent = h.request_messages(6).await;
    let contents: Vec<&Value> = parent.iter().map(|m| &m["content"]).collect();
    assert_eq!(parent.len(), 6, "{contents:?}");
    assert_eq!(parent[3]["content"], "report one");
    assert_eq!(parent[5]["content"], "report two");
    assert!(
        !contents
            .iter()
            .any(|c| c.as_str() == Some("You do one step."))
    );
    assert_eq!(h.agent.as_ref().unwrap().model_swaps(), 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worker_on_another_model_obeys_the_session_deny_rules() {
    let script = vec![
        tool_reply(&[(
            "p1",
            "Agent",
            json!({"subagent_type": "kit:step", "prompt": "write"}),
        )]),
        tool_reply(&[("s1", "Write", json!({"file_path": "x", "content": "y"}))]),
        text_reply("refused"),
        text_reply("done"),
    ];
    let workspace = new_workspace();
    let policy = harness_core::permissions::Policy::new(
        harness_core::permissions::Mode::Ask,
        &[],
        &["Write".to_owned()],
    )
    .unwrap();
    let mut h = Harness::with_policy(workspace.clone(), script, 50, policy).await;
    serve_worker_model(&h.server).await;
    let step = lmstudio_worker("kit:step", vec![]);
    h.agent = Some(h.agent.take().unwrap().with_agents(vec![step]));

    let events = h
        .turn("go", CancellationToken::new(), |_| {
            panic!("a denied call must not ask")
        })
        .await;

    assert!(events.contains(&"error:s1".to_owned()), "{events:?}");
    assert!(!workspace.join("x").exists());
}
