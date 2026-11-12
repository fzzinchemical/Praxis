//! Plan-first (SPEC §11, T5.9): while the agent is in read-only plan mode nothing can be written
//! or run, even with rules that allow it. Only the developer's switch back to the configured
//! mode lets a call reach the approval path.

// Lets clippy treat helper functions here as test code for `allow-unwrap-in-tests`.
#![cfg(test)]

use harness_core::agent::profile::ModelProfile;
use harness_core::agent::{Agent, AgentConfig, AgentEvent, PermissionReply, ToolOutcome};
use harness_core::permissions::{Mode, Policy};
use harness_core::provider::Provider;
use harness_core::tools::ToolContext;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate, matchers};

/// The widest rules a user could write, so only the mode stands in the way.
const ALLOW_ALL: [&str; 3] = ["Write", "Edit", "Bash"];

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
    ])
}

fn tool_reply(name: &str, args: &Value) -> String {
    let call =
        json!({"index": 0, "id": "c", "function": {"name": name, "arguments": args.to_string()}});
    sse(&[
        json!({"choices": [{"delta": {"tool_calls": [call]}, "finish_reason": null}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

fn temp_dir(kind: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("harness-plan-{kind}-{}-{id}", std::process::id()));
    fs::create_dir_all(&path).unwrap();
    path.canonicalize().unwrap()
}

/// An agent whose model makes one call per turn, in order, then answers with text.
async fn make_agent(workspace: &Path, mode: Mode, calls: &[(&str, Value)]) -> (MockServer, Agent) {
    let server = MockServer::start().await;
    // Each turn is a tool call and then the text reply that follows its result.
    let bodies = calls
        .iter()
        .flat_map(|(name, args)| [tool_reply(name, args), text_reply("ok")])
        .collect();
    Mock::given(matchers::method("POST"))
        .respond_with(Script {
            bodies,
            next: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;
    let allow: Vec<String> = ALLOW_ALL.map(String::from).to_vec();
    let config = AgentConfig {
        model: "m".into(),
        context_size: 100_000,
        system_prompt: "sys".into(),
        agents_md: None,
        mask_keep_turns: 10,
        max_turn_requests: 5,
        profile: ModelProfile::default(),
        seed: None,
        policy: Policy::new(mode, &allow, &[]).unwrap(),
        masking: true,
    };
    let provider = Provider::new(&server.uri(), None).unwrap();
    let tools = ToolContext::new(workspace).unwrap();
    let agent = Agent::new(Arc::new(provider), Arc::new(tools), config);
    (server, agent)
}

/// Runs one turn, answering every prompt with `approve`. Returns the agent, the number of
/// prompts raised, and the number of calls the tools refused.
async fn run_turn(mut agent: Agent, approve: bool) -> (Agent, usize, usize) {
    let (tx, mut rx) = mpsc::channel(16);
    let task = tokio::spawn(async move {
        agent
            .run_turn("go".into(), &tx, CancellationToken::new())
            .await;
        agent
    });
    let (mut asked, mut refused) = (0, 0);
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::PermissionRequest { respond, .. } => {
                asked += 1;
                let reply = if approve {
                    PermissionReply::AllowOnce
                } else {
                    PermissionReply::RejectOnce
                };
                let _ = respond.send(reply);
            }
            AgentEvent::ToolFinished {
                outcome: ToolOutcome::Completed(output),
                ..
            } => refused += usize::from(output.is_error),
            _ => {}
        }
    }
    (task.await.unwrap(), asked, refused)
}

#[tokio::test(flavor = "multi_thread")]
async fn plan_mode_writes_and_runs_nothing_even_with_allow_rules() {
    let workspace = temp_dir("ws");
    let target = workspace.join("made.txt");
    let calls = [
        ("Write", json!({"file_path": target, "content": "x"})),
        (
            "Bash",
            json!({"command": format!("touch {}", target.display())}),
        ),
    ];
    for call in calls {
        let (_server, mut agent) =
            make_agent(&workspace, Mode::ReadOnly, std::slice::from_ref(&call)).await;
        agent.set_mode(Mode::ReadOnly);
        let (_, asked, refused) = run_turn(agent, true).await;
        assert_eq!(asked, 0, "{} must be refused, not offered", call.0);
        assert_eq!(refused, 1, "{} must come back refused", call.0);
        assert!(!target.exists(), "{} wrote during plan mode", call.0);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn carrying_out_a_plan_needs_the_developers_switch_back() {
    let workspace = temp_dir("ws");
    let target = workspace.join("made.txt");
    let write = ("Write", json!({"file_path": target, "content": "x"}));
    let (_server, mut agent) = make_agent(
        &workspace,
        Mode::Ask,
        &[write.clone(), write.clone(), write],
    )
    .await;

    // The plan turn: the model's write is refused without a prompt, and nothing else in the
    // agent can leave plan mode.
    agent.set_mode(Mode::ReadOnly);
    let (agent, asked, refused) = run_turn(agent, true).await;
    assert_eq!((asked, refused), (0, 1));
    assert!(!target.exists());

    // Still planning until the developer acts: the same write is refused again.
    let (mut agent, asked, refused) = run_turn(agent, true).await;
    assert_eq!((asked, refused), (0, 1));
    assert!(!target.exists());

    // The developer's switch back to the configured mode lets the write through, since the
    // configured rules allow it.
    agent.set_mode(Mode::Ask);
    let (_, asked, refused) = run_turn(agent, true).await;
    assert_eq!((asked, refused), (0, 0));
    assert!(target.exists());
}
