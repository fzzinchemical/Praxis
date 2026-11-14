//! US-62 acceptance tests: listing, deleting and moving saved sessions from the terminal.

// Clippy's `allow-unwrap-in-tests` only applies to helpers in files that start with this.
#![cfg(test)]

// This binary uses only some shared helpers; the rest are dead code here, not in the suite.
#[allow(dead_code, unused_imports)]
mod common;

use common::*;
use serde_json::Value;
use std::path::Path;
use std::process::Output;

/// Nothing listens here, so any command that reaches LM Studio fails the test.
const NO_LM_STUDIO: &str = "http://127.0.0.1:9";

/// Runs a one-shot prompt in `dir`, saving its session under the shared data directory `data`.
async fn save_session(dir: &Path, data: &Path, prompt: &str) -> Output {
    let server = server(Some(32768), Replies(vec![text_reply("ok")])).await;
    command(&server.uri(), dir, &["--model", "m", prompt])
        .env("XDG_DATA_HOME", data)
        .output()
        .await
        .unwrap()
}

/// `praxis sessions <args>` in `dir`, reading the shared data directory `data`.
async fn sessions(dir: &Path, data: &Path, args: &[&str]) -> Output {
    let mut all = vec!["sessions"];
    all.extend_from_slice(args);
    command(NO_LM_STUDIO, dir, &all)
        .env("XDG_DATA_HOME", data)
        .output()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_lists_one_line_per_session_of_this_workspace() {
    let data = workspace().join("data");
    let ours = workspace();
    let theirs = workspace();
    let saved = save_session(&ours, &data, "our prompt").await;
    assert_eq!(saved.status.code(), Some(0), "{}", err(&saved));
    let saved = save_session(&theirs, &data, "their prompt").await;
    assert_eq!(saved.status.code(), Some(0), "{}", err(&saved));

    let listed = sessions(&ours, &data, &[]).await;

    assert_eq!(listed.status.code(), Some(0), "{}", err(&listed));
    let text = out(&listed);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "{text}");
    let (id, title) = lines[0].split_once(' ').unwrap();
    assert!(!id.is_empty(), "{text}");
    assert_eq!(title, "our prompt", "{text}");
    assert!(!text.contains("their prompt"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_prints_titles_without_terminal_escapes() {
    let data = workspace().join("data");
    let dir = workspace();
    let saved = save_session(&dir, &data, "\u{1b}[31mred").await;
    assert_eq!(saved.status.code(), Some(0), "{}", err(&saved));

    let listed = sessions(&dir, &data, &[]).await;

    assert_eq!(listed.status.code(), Some(0), "{}", err(&listed));
    let text = out(&listed);
    assert!(!text.contains('\u{1b}'), "{text:?}");
    assert!(text.contains("red"), "{text:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_delete_removes_only_the_named_session() {
    let data = workspace().join("data");
    let dir = workspace();
    let saved = save_session(&dir, &data, "delete me").await;
    assert_eq!(saved.status.code(), Some(0), "{}", err(&saved));
    let saved = save_session(&dir, &data, "keep me").await;
    assert_eq!(saved.status.code(), Some(0), "{}", err(&saved));
    let listing = sessions(&dir, &data, &["--json"]).await;
    let rows: Value = serde_json::from_str(&out(&listing)).unwrap();
    let doomed = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["title"] == "delete me")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let deleted = sessions(&dir, &data, &["--delete", &doomed]).await;

    assert_eq!(deleted.status.code(), Some(0), "{}", err(&deleted));
    let listing = sessions(&dir, &data, &["--json"]).await;
    let rows: Value = serde_json::from_str(&out(&listing)).unwrap();
    assert_eq!(rows.as_array().map(Vec::len), Some(1), "{rows}");
    assert_eq!(rows[0]["title"], "keep me", "{rows}");
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_folder_with_one_argument_exits_with_usage_code() {
    let dir = workspace();

    let output = command(NO_LM_STUDIO, &dir, &["sessions", "--folder", "some-id"])
        .output()
        .await
        .unwrap();

    assert_eq!(output.status.code(), Some(2), "{}", err(&output));
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_without_home_or_data_dir_says_so_and_exits_with_usage_code() {
    let dir = workspace();

    let output = command(NO_LM_STUDIO, &dir, &["sessions"])
        .env_remove("HOME")
        .env_remove("XDG_DATA_HOME")
        .output()
        .await
        .unwrap();

    assert_eq!(output.status.code(), Some(2), "{}", err(&output));
    assert!(
        err(&output).contains("Sessions need HOME or XDG_DATA_HOME"),
        "{}",
        err(&output)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn run_without_home_or_data_dir_warns_the_session_isnt_saved_and_still_replies() {
    let dir = workspace();
    let server = server(Some(32768), Replies(vec![text_reply("hello back")])).await;

    let output = command(&server.uri(), &dir, &["--model", "m", "hi"])
        .env_remove("HOME")
        .env_remove("XDG_DATA_HOME")
        .output()
        .await
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    assert!(
        err(&output).contains("this session isn't saved"),
        "{}",
        err(&output)
    );
    assert!(out(&output).contains("hello back"), "{}", out(&output));
}
