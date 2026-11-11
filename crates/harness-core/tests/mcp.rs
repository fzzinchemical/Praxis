//! The stdio MCP client against a small `rmcp` server (SPEC §13, §14). This binary is also
//! that server: run with `serve`, it answers MCP on stdio.

// `harness = false` builds without `cfg(test)`, so clippy can't tell this is test code: a
// failed unwrap here is a failed test, as in every other test file.
#![allow(clippy::unwrap_used)]

use harness_core::agent::{Agent, AgentConfig, AgentEvent, PermissionReply, ToolOutcome};
use harness_core::config::McpServer;
use harness_core::mcp::Mcp;
use harness_core::provider::Provider;
use harness_core::tools::ToolContext;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt as _};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers};

struct Echo;

impl ServerHandler for Echo {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let schema = json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]});
        let Value::Object(schema) = schema else {
            unreachable!()
        };
        Ok(ListToolsResult::with_all_items(vec![Tool::new(
            "echo",
            "Echo text back.",
            schema,
        )]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let text = request
            .arguments
            .and_then(|a| a.get("text").and_then(Value::as_str).map(str::to_owned));
        Ok(
            CallToolResult::success(vec![ContentBlock::text(format!("echo: {}", text.unwrap()))])
                .into(),
        )
    }
}

fn server(command: &str, args: &[&str]) -> McpServer {
    McpServer {
        command: command.to_owned(),
        args: args.iter().map(|a| (*a).to_owned()).collect(),
    }
}

fn servers() -> BTreeMap<String, McpServer> {
    let me = std::env::current_exe().unwrap();
    BTreeMap::from([
        ("test".to_owned(), server(me.to_str().unwrap(), &["serve"])),
        ("broken".to_owned(), server("/nonexistent/mcp-server", &[])),
        // Starts but never speaks MCP.
        ("slow".to_owned(), server("sleep", &["30"])),
    ])
}

async fn slow_or_failing_servers_dont_block_startup() {
    let start = Instant::now();
    let (mcp, notices) = Mcp::connect(&servers(), Duration::from_secs(2)).await;
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    let names: Vec<String> = mcp.schemas().into_iter().map(|s| s.name).collect();
    assert_eq!(names, ["mcp__test__echo"]);
    let all = notices.join("\n");
    assert!(all.contains("`broken`") && all.contains("`slow`"), "{all}");
}

fn sse(chunks: &[Value]) -> String {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    body
}

async fn mcp_tools_are_callable_through_the_agent_loop() {
    let model = MockServer::start().await;
    let call = json!({"index": 0, "id": "c1", "function": {"name": "mcp__test__echo", "arguments": "{\"text\":\"hi\"}"}});
    Mock::given(matchers::method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&[
                json!({"choices": [{"delta": {"tool_calls": [call]}, "finish_reason": null}]}),
                json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
            ]),
            "text/event-stream",
        ))
        .up_to_n_times(1)
        .mount(&model)
        .await;
    Mock::given(matchers::method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&[json!({"choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]})]),
            "text/event-stream",
        ))
        .mount(&model)
        .await;
    let mut servers = servers();
    servers.retain(|name, _| name == "test");
    let (mcp, _) = Mcp::connect(&servers, Duration::from_secs(10)).await;
    let mut agent = Agent::new(
        Arc::new(Provider::new(&model.uri(), None).unwrap()),
        Arc::new(ToolContext::new(&std::env::temp_dir()).unwrap()),
        AgentConfig {
            model: "m".into(),
            context_size: 100_000,
            system_prompt: "sys".into(),
            agents_md: None,
            mask_keep_turns: 10,
            max_turn_requests: 5,
            profile: harness_core::agent::profile::ModelProfile::default(),
            seed: None,
            policy: harness_core::permissions::Policy::default(),
            masking: true,
        },
    )
    .with_mcp(Arc::new(mcp));
    let (tx, mut rx) = mpsc::channel(16);
    let task = tokio::spawn(async move {
        agent
            .run_turn("go".into(), &tx, CancellationToken::new())
            .await;
    });
    let (mut asked, mut output) = (false, None);
    while let Some(event) = rx.recv().await {
        match event {
            // MCP tools ask before running, like any tool that isn't read-only.
            AgentEvent::PermissionRequest { respond, .. } => {
                asked = true;
                respond.send(PermissionReply::AllowOnce).unwrap();
            }
            AgentEvent::ToolFinished {
                outcome: ToolOutcome::Completed(out),
                ..
            } => {
                output = Some(out);
            }
            _ => {}
        }
    }
    task.await.unwrap();
    assert!(asked);
    let output = output.unwrap();
    assert!(!output.is_error, "{}", output.content);
    assert_eq!(output.content, "echo: hi");
    let requests = model.received_requests().await.unwrap();
    let first: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(first["tools"].to_string().contains("mcp__test__echo"));
}

fn main() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    if std::env::args().nth(1).as_deref() == Some("serve") {
        runtime.block_on(async {
            let running = Echo.serve(rmcp::transport::io::stdio()).await.unwrap();
            running.waiting().await.unwrap();
        });
        return;
    }
    runtime.block_on(slow_or_failing_servers_dont_block_startup());
    println!("slow_or_failing_servers_dont_block_startup ... ok");
    runtime.block_on(mcp_tools_are_callable_through_the_agent_loop());
    println!("mcp_tools_are_callable_through_the_agent_loop ... ok");
}
