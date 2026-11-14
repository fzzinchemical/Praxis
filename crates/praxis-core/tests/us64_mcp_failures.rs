//! US-64 acceptance: every MCP failure gives a clear tool result, and an unreadable trust store
//! hides the server's tools with a notice naming the file. This binary is also the small
//! `rmcp` server these tests start, run with `serve`.

// `praxis = false` builds without `cfg(test)`, so clippy can't tell this is test code: a
// failed unwrap here is a failed test, as in every other test file.
#![allow(clippy::unwrap_used)]

use praxis_core::config::{McpServer, StdioServer};
use praxis_core::mcp::Mcp;
use praxis_core::trust::TrustStore;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt as _};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

struct Story;

impl ServerHandler for Story {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let schema = json!({"type": "object", "properties": {}});
        let Value::Object(schema) = schema else {
            unreachable!()
        };
        Ok(ListToolsResult::with_all_items(
            ["hang", "media"]
                .into_iter()
                .map(|name| Tool::new(name, "Story test tool.", schema.clone()))
                .collect(),
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        match &*request.name {
            // Never answers: the client's call timeout is the only way out.
            "hang" => std::future::pending().await,
            "media" => Ok(CallToolResult::success(vec![
                ContentBlock::image("SECRETIMAGEBYTES", "image/png"),
                ContentBlock::audio("SECRETAUDIOBYTES", "audio/wav"),
                ContentBlock::embedded_text("file:///secret.txt", "SECRETRESOURCEBYTES"),
            ])
            .into()),
            other => Err(ErrorData::invalid_params(format!("no tool {other}"), None)),
        }
    }
}

fn story_servers() -> BTreeMap<String, McpServer> {
    let me = std::env::current_exe().unwrap();
    BTreeMap::from([(
        "story".to_owned(),
        McpServer::Stdio(StdioServer {
            command: me.to_str().unwrap().to_owned(),
            args: vec!["serve".to_owned()],
            env: BTreeMap::new(),
        }),
    )])
}

/// Connects to the story server and checks it is up, so a failing server can't make the
/// assertions below pass for the wrong reason.
async fn connected() -> Mcp {
    let (mcp, notices) = Mcp::connect(&story_servers(), Duration::from_secs(10)).await;
    assert!(notices.is_empty(), "{notices:?}");
    let names: Vec<String> = mcp.schemas().into_iter().map(|s| s.name).collect();
    assert!(names.contains(&"mcp__story__media".to_owned()), "{names:?}");
    mcp
}

async fn an_unknown_tool_gives_an_unknown_tool_result() {
    let mcp = connected().await;
    let result = mcp
        .call("mcp__story__missing", json!({}), CancellationToken::new())
        .await;
    assert!(result.is_error, "{}", result.content);
    assert!(
        result.content.contains("Unknown tool"),
        "{}",
        result.content
    );
}

/// Runs on a paused clock, so the 60 s call timeout fires once the runtime is idle, not after
/// a real minute.
async fn a_tool_that_never_answers_times_out_after_60_seconds() {
    let mcp = connected().await;
    tokio::time::pause();
    let result = mcp
        .call("mcp__story__hang", json!({}), CancellationToken::new())
        .await;
    assert!(result.is_error, "{}", result.content);
    assert!(
        result.content.contains("gave no answer within 60s"),
        "{}",
        result.content
    );
}

async fn image_audio_and_resource_content_gives_placeholders_not_data() {
    let mcp = connected().await;
    let result = mcp
        .call("mcp__story__media", json!({}), CancellationToken::new())
        .await;
    assert!(!result.is_error, "{}", result.content);
    for placeholder in ["[image omitted]", "[audio omitted]", "[resource omitted]"] {
        assert!(result.content.contains(placeholder), "{}", result.content);
    }
    for secret in [
        "SECRETIMAGEBYTES",
        "SECRETAUDIOBYTES",
        "SECRETRESOURCEBYTES",
    ] {
        assert!(!result.content.contains(secret), "{}", result.content);
    }
}

async fn an_unreadable_trust_store_hides_the_tools_and_names_the_file() {
    let dir = std::env::temp_dir().join(format!("praxis-us64-trust-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // A directory where the trust file should be: reading it fails, and it can't be approved.
    let trust = dir.join("trust.json");
    std::fs::create_dir_all(&trust).unwrap();
    let store = TrustStore::new(trust.clone());

    let (mut mcp, _) = Mcp::connect(&story_servers(), Duration::from_secs(10)).await;
    let notices = mcp.pin(Some(&store), &mut |_| {
        panic!("an unreadable trust store must not ask for approval")
    });
    assert!(mcp.schemas().is_empty(), "{:?}", mcp.schemas());
    let file = trust.display().to_string();
    assert!(
        notices
            .iter()
            .any(|n| n.contains("is hidden") && n.contains(&file)),
        "{notices:?}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("serve") {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let running = Story.serve(rmcp::transport::io::stdio()).await.unwrap();
            running.waiting().await.unwrap();
        });
        return;
    }
    let multi = tokio::runtime::Runtime::new().unwrap();
    multi.block_on(an_unknown_tool_gives_an_unknown_tool_result());
    println!("an_unknown_tool_gives_an_unknown_tool_result ... ok");
    // Current-thread, so pausing the clock is the only thing that moves time.
    let single = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    single.block_on(a_tool_that_never_answers_times_out_after_60_seconds());
    println!("a_tool_that_never_answers_times_out_after_60_seconds ... ok");
    multi.block_on(image_audio_and_resource_content_gives_placeholders_not_data());
    println!("image_audio_and_resource_content_gives_placeholders_not_data ... ok");
    multi.block_on(an_unreadable_trust_store_hides_the_tools_and_names_the_file());
    println!("an_unreadable_trust_store_hides_the_tools_and_names_the_file ... ok");
}
