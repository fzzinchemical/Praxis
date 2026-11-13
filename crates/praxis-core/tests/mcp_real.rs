//! Real filesystem, git and browser MCP servers (SPEC §13, T4.5). They are not in the repo:
//! install them first (see docs/MCP_SERVERS.md), put their bin dirs on PATH, then run
//! `cargo test -p praxis-core --test mcp_real -- --ignored --test-threads=1`.

#![allow(clippy::unwrap_used)]

use praxis_core::config::{McpServer, StdioServer};
use praxis_core::mcp::Mcp;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn stdio(command: &str, args: &[&str]) -> McpServer {
    McpServer::Stdio(StdioServer {
        command: command.to_owned(),
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        env: BTreeMap::new(),
    })
}

/// A fresh directory for one test, with one git commit so the git server has a repo to read.
fn workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mcp-real-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("notes.txt"), "hello from the filesystem server\n").unwrap();
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(&dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_WORK_TREE")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@example.invalid",
        "add",
        ".",
    ]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@example.invalid",
        "commit",
        "-qm",
        "init",
    ]);
    dir
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the real MCP servers on PATH (docs/MCP_SERVERS.md)"]
async fn real_servers_connect_and_answer_tool_calls() {
    let dir = workspace("servers");
    let servers = BTreeMap::from([
        (
            "filesystem".to_owned(),
            stdio("mcp-server-filesystem", &[dir.to_str().unwrap()]),
        ),
        ("git".to_owned(), stdio("mcp-server-git", &[])),
        (
            "browser".to_owned(),
            stdio(
                "playwright-mcp",
                &[
                    "--headless",
                    "--browser",
                    "chromium",
                    "--output-dir",
                    dir.to_str().unwrap(),
                ],
            ),
        ),
    ]);

    let (mcp, notices) = Mcp::connect(&servers, Duration::from_secs(60)).await;
    assert!(notices.is_empty(), "{notices:?}");
    let names: Vec<String> = mcp.schemas().into_iter().map(|t| t.name).collect();
    eprintln!("{} tools: {names:?}", names.len());
    assert!(names.iter().any(|n| n.starts_with("mcp__filesystem__")));
    assert!(names.iter().any(|n| n.starts_with("mcp__git__")));
    assert!(names.iter().any(|n| n.starts_with("mcp__browser__")));

    let cancel = CancellationToken::new();
    let read = mcp
        .call(
            "mcp__filesystem__read_text_file",
            json!({"path": dir.join("notes.txt")}),
            cancel.clone(),
        )
        .await;
    assert!(!read.is_error, "{}", read.content);
    assert!(
        read.content.contains("hello from the filesystem server"),
        "{}",
        read.content
    );

    let status = mcp
        .call(
            "mcp__git__git_status",
            json!({"repo_path": dir}),
            cancel.clone(),
        )
        .await;
    assert!(!status.is_error, "{}", status.content);
    assert!(status.content.contains("nothing to commit") || status.content.contains("On branch"));

    let page = mcp
        .call(
            "mcp__browser__browser_navigate",
            json!({"url": "data:text/html,<h1>Harness check</h1>"}),
            cancel,
        )
        .await;
    assert!(!page.is_error, "{}", page.content);
    assert!(page.content.contains("Harness check"), "{}", page.content);

    let _ = fs::remove_dir_all(&dir);
}
