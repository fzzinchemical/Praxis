//! Token budget, stable prefix and compaction against a scripted fake server (SPEC §7, §14).

// Lets clippy treat helper functions here as test code for `allow-unwrap-in-tests`.
#![cfg(test)]

use harness_core::agent::{Agent, AgentConfig, AgentEvent, ToolOutcome};
use harness_core::provider::Provider;
use harness_core::tools::{self, ToolContext};
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate, matchers};

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

fn text_reply(text: &str, prompt_tokens: u32) -> String {
    sse(&[
        json!({"choices": [{"delta": {"content": text}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": 2}}),
    ])
}

fn tool_reply(id: &str, name: &str, args: Value) -> String {
    let call =
        json!({"index": 0, "id": id, "function": {"name": name, "arguments": args.to_string()}});
    sse(&[json!({"choices": [{"delta": {"tool_calls": [call]}, "finish_reason": "tool_calls"}]})])
}

fn new_workspace() -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("harness-context-{}-{id}", std::process::id()));
    fs::create_dir_all(&path).unwrap();
    path.canonicalize().unwrap()
}

fn config(context_size: u32) -> AgentConfig {
    AgentConfig {
        model: "m".into(),
        context_size,
        system_prompt: "sys".into(),
        agents_md: None,
        max_turn_requests: 50,
        profile: harness_core::agent::profile::ModelProfile::default(),
    }
}

struct Harness {
    server: MockServer,
    workspace: PathBuf,
    agent: Option<Agent>,
}

impl Harness {
    async fn new(bodies: Vec<String>, config: AgentConfig) -> Self {
        let workspace = new_workspace();
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
            config,
        );
        Self {
            server,
            workspace,
            agent: Some(agent),
        }
    }

    async fn turn(&mut self, prompt: &str) -> Vec<AgentEvent> {
        let mut agent = self.agent.take().unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let prompt = prompt.to_owned();
        let task = tokio::spawn(async move {
            agent.run_turn(prompt, &tx, CancellationToken::new()).await;
            agent
        });
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        self.agent = Some(task.await.unwrap());
        events
    }

    async fn bodies(&self) -> Vec<Value> {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }
}

fn warnings(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Notice { title, .. } => Some(title.clone()),
            _ => None,
        })
        .collect()
}

fn big_file_lines(lines: usize) -> String {
    (1..=lines).map(|n| format!("line number {n}\n")).collect()
}

fn last_message(body: &Value) -> String {
    body["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn reserves_fifteen_percent_of_the_context_for_output() {
    let mut h = Harness::new(vec![text_reply("ok", 3000)], config(20_000)).await;
    h.turn("hi").await;
    assert_eq!(h.bodies().await[0]["max_tokens"], 3000);
}

#[tokio::test(flavor = "multi_thread")]
async fn spools_an_oversized_result_and_keeps_head_and_tail() {
    let bodies = vec![
        tool_reply("a", "Read", json!({"file_path": "big.txt"})),
        text_reply("done", 4000),
    ];
    let mut h = Harness::new(bodies, config(20_000)).await;
    fs::write(h.workspace.join("big.txt"), big_file_lines(1500)).unwrap();
    h.turn("read it").await;

    let result = last_message(&h.bodies().await[1]);
    // 10% of 20000 tokens at the uncalibrated 3.5 chars per token.
    assert!(result.chars().count() <= 7000, "{}", result.len());
    assert!(result.contains("line number 1\n"), "keeps the head");
    assert!(result.contains("line number 1500\n"), "keeps the tail");
    assert!(!result.contains("line number 700\n"), "drops the middle");

    let spooled = result
        .split_whitespace()
        .find(|word| word.contains("harness-spool"))
        .unwrap();
    let raw = fs::read_to_string(spooled).unwrap();
    assert!(raw.contains("line number 700\n"), "full text on disk");

    let other_session = Arc::new(ToolContext::new(&h.workspace).unwrap());
    let args = json!({"file_path": spooled});
    let cancel = CancellationToken::new();
    let denied = tools::execute("Read", args, other_session, cancel).await;
    assert!(
        denied.is_error,
        "another session's spool stays out of reach"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_agent_reads_its_own_spool_but_cannot_write_there() {
    let dir = new_workspace();
    let ctx = Arc::new(ToolContext::new(&dir).unwrap());
    let path = ctx.spool("spooled text\n").unwrap();
    let raw = path.to_str().unwrap();
    let cancel = CancellationToken::new;

    let args = json!({"file_path": raw, "offset": 1, "limit": 5});
    let read = tools::execute("Read", args, Arc::clone(&ctx), cancel()).await;
    assert!(
        !read.is_error && read.content.contains("spooled text"),
        "{}",
        read.content
    );

    let args = json!({"file_path": raw, "content": "x"});
    let write = tools::execute("Write", args, Arc::clone(&ctx), cancel()).await;
    assert!(write.is_error, "{}", write.content);

    let spool_dir = path.parent().unwrap().to_owned();
    drop(ctx);
    assert!(!spool_dir.exists(), "the spool goes with the session");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_call_id_cannot_steer_the_spool_path() {
    let bodies = vec![
        tool_reply("../../escape", "Read", json!({"file_path": "big.txt"})),
        text_reply("done", 4000),
    ];
    let mut h = Harness::new(bodies, config(20_000)).await;
    fs::write(h.workspace.join("big.txt"), big_file_lines(1500)).unwrap();
    h.turn("read it").await;
    let result = last_message(&h.bodies().await[1]);
    assert!(result.contains("harness-spool"));
    assert!(!result.contains("escape"));
}

#[tokio::test(flavor = "multi_thread")]
async fn trims_agents_md_and_warns_when_the_prefix_is_over_budget() {
    let mut cfg = config(8000);
    cfg.agents_md = Some(format!("KEEP-THE-START {}", "x".repeat(30_000)));
    let mut h = Harness::new(vec![text_reply("ok", 2000)], cfg).await;
    let events = h.turn("hi").await;
    assert!(warnings(&events).iter().any(|w| w.contains("AGENTS.md")));

    let bodies = h.bodies().await;
    let system = bodies[0]["messages"][0]["content"].as_str().unwrap();
    assert!(system.starts_with("sys") && system.contains("KEEP-THE-START"));
    let sent = h.server.received_requests().await.unwrap()[0].body.len();
    // 25% of 8000 tokens at 3.5 chars each, plus a little for the prompt and JSON around it.
    assert!(sent <= 7000 + 200, "sent {sent} chars");
}

#[tokio::test(flavor = "multi_thread")]
async fn keeps_a_small_agents_md_whole_without_a_warning() {
    let mut cfg = config(100_000);
    cfg.agents_md = Some("Use tabs.".into());
    let mut h = Harness::new(vec![text_reply("ok", 3000)], cfg).await;
    let events = h.turn("hi").await;
    assert!(warnings(&events).is_empty(), "{:?}", warnings(&events));
    let bodies = h.bodies().await;
    let system = bodies[0]["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("Use tabs."));
}

#[tokio::test(flavor = "multi_thread")]
async fn warns_when_the_server_reports_a_far_smaller_prompt_than_estimated() {
    let mut h = Harness::new(vec![text_reply("ok", 5)], config(100_000)).await;
    let events = h.turn("hi").await;
    assert!(
        warnings(&events).iter().any(|w| w.contains("truncated")),
        "{:?}",
        warnings(&events)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_front_end_still_gets_the_complete_result() {
    let bodies = vec![
        tool_reply("a", "Read", json!({"file_path": "big.txt"})),
        text_reply("done", 4000),
    ];
    let mut h = Harness::new(bodies, config(20_000)).await;
    fs::write(h.workspace.join("big.txt"), big_file_lines(1500)).unwrap();
    let events = h.turn("read").await;
    let shown = events.iter().find_map(|e| match e {
        AgentEvent::ToolFinished {
            outcome: ToolOutcome::Completed(output),
            ..
        } => Some(output.content.clone()),
        _ => None,
    });
    assert!(shown.unwrap().contains("line number 700\n"));
}
