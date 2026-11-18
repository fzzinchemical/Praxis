//! US-61 acceptance tests: `/plan`, `/execute`, `/undo` and `/rewind` in the terminal REPL.

// Lets clippy treat helper functions here as test code for `allow-unwrap-in-tests`.
#![cfg(test)]

mod common;

use common::*;
use praxis_core::tools::UNDO_NOTE;
use serde_json::json;
use std::fs;

const NO_LOAD_ARGS: &str = "--config-override agent.auto_load=false --model m";

#[tokio::test(flavor = "multi_thread")]
async fn execute_without_a_plan_says_so_and_sends_nothing_to_the_model() {
    let dir = workspace();
    let server = server(None, Replies(vec![text_reply("should not be used")])).await;

    let output = in_terminal(&server.uri(), &dir, NO_LOAD_ARGS, "y\n/execute\n/exit\n").await;

    assert_eq!(output.status.code(), Some(0), "{}", out(&output));
    assert!(
        out(&output).contains("Nothing to execute; start with /plan <task>."),
        "{}",
        out(&output)
    );
    assert!(requests_to(&server, CHAT).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_slash_command_is_reported_and_not_sent_to_the_model() {
    let dir = workspace();
    let server = server(None, Replies(vec![text_reply("should not be used")])).await;

    let typed = "y\n/nosuchcommand now\n/exit\n";
    let output = in_terminal(&server.uri(), &dir, NO_LOAD_ARGS, typed).await;

    assert_eq!(output.status.code(), Some(0), "{}", out(&output));
    assert!(
        out(&output).contains("Unknown command: /nosuchcommand now"),
        "{}",
        out(&output)
    );
    assert!(requests_to(&server, CHAT).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_prompt_starting_with_a_path_is_sent_to_the_model() {
    let dir = workspace();
    let server = server(None, Replies(vec![text_reply("ok")])).await;

    let typed = "y\n/tmp/x.rs fails to build\n/exit\n";
    let output = in_terminal(&server.uri(), &dir, NO_LOAD_ARGS, typed).await;

    assert_eq!(output.status.code(), Some(0), "{}", out(&output));
    assert!(
        !out(&output).contains("Unknown command"),
        "{}",
        out(&output)
    );
    assert_eq!(requests_to(&server, CHAT).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn plan_without_a_task_prints_usage_and_sends_nothing_to_the_model() {
    let dir = workspace();
    let server = server(None, Replies(vec![text_reply("should not be used")])).await;

    let output = in_terminal(&server.uri(), &dir, NO_LOAD_ARGS, "y\n/plan\n/exit\n").await;

    assert_eq!(output.status.code(), Some(0), "{}", out(&output));
    assert!(
        out(&output).contains("Usage: /plan <task>"),
        "{}",
        out(&output)
    );
    assert!(requests_to(&server, CHAT).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn plan_runs_read_only_so_a_scripted_write_is_refused_and_the_task_is_sent() {
    let dir = workspace();
    let refused = dir.join("planned.txt");
    let server = server(
        None,
        Replies(vec![
            tool_reply("Write", &json!({"file_path": refused, "content": "x"})),
            text_reply("here is the plan"),
        ]),
    )
    .await;

    let typed = "y\n/plan add a cache layer\n/exit\n";
    let output = in_terminal(&server.uri(), &dir, NO_LOAD_ARGS, typed).await;

    assert_eq!(output.status.code(), Some(0), "{}", out(&output));
    assert!(!refused.exists());
    let chats = requests_to(&server, CHAT).await;
    assert!(
        chats[0]["messages"]
            .to_string()
            .contains("add a cache layer")
    );
    // A rejected approval also leaves no file, so check the model was told why.
    let follow_up = chats[1]["messages"].to_string();
    assert!(
        follow_up.contains("can't run in read-only mode"),
        "{follow_up}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn execute_after_plan_restores_the_configured_mode_so_a_write_proceeds() {
    let dir = workspace();
    let written = dir.join("executed.txt");
    let server = server(
        None,
        Replies(vec![
            text_reply("the plan"),
            tool_reply("Write", &json!({"file_path": written, "content": "done"})),
            text_reply("carried out"),
        ]),
    )
    .await;

    // The "y" after `/execute` answers the write approval, if one is asked.
    let typed = "y\n/plan write the file\n/execute\ny\n/exit\n";
    let output = in_terminal(&server.uri(), &dir, NO_LOAD_ARGS, typed).await;

    assert_eq!(output.status.code(), Some(0), "{}", out(&output));
    assert_eq!(fs::read_to_string(&written).unwrap(), "done");
    let chats = requests_to(&server, CHAT).await;
    assert!(
        chats.last().unwrap()["messages"]
            .to_string()
            .contains("Carry out the plan above now.")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn undo_with_nothing_to_undo_says_so_and_names_the_bash_limit() {
    let dir = workspace();
    let server = server(None, Replies(vec![text_reply("unused")])).await;

    let output = in_terminal(&server.uri(), &dir, NO_LOAD_ARGS, "y\n/undo\n/exit\n").await;

    assert_eq!(output.status.code(), Some(0), "{}", out(&output));
    assert!(
        out(&output).contains("Nothing to undo."),
        "{}",
        out(&output)
    );
    assert!(out(&output).contains(UNDO_NOTE), "{}", out(&output));
}

#[tokio::test(flavor = "multi_thread")]
async fn undo_after_an_approved_write_restores_the_file_and_says_so() {
    let dir = workspace();
    let edited = dir.join("edited.txt");
    fs::write(&edited, "original").unwrap();
    let server = server(
        None,
        Replies(vec![
            tool_reply("Write", &json!({"file_path": edited, "content": "changed"})),
            text_reply("wrote it"),
        ]),
    )
    .await;

    let typed = "y\nchange it\ny\n/undo\n/exit\n";
    let output = in_terminal(&server.uri(), &dir, NO_LOAD_ARGS, typed).await;

    assert_eq!(output.status.code(), Some(0), "{}", out(&output));
    assert_eq!(fs::read_to_string(&edited).unwrap(), "original");
    assert!(out(&output).contains("Restored "), "{}", out(&output));
    assert!(
        out(&output).contains(edited.to_str().unwrap()),
        "{}",
        out(&output)
    );
    assert!(out(&output).contains(UNDO_NOTE), "{}", out(&output));
}

#[tokio::test(flavor = "multi_thread")]
async fn rewind_with_bad_or_out_of_range_numbers_prints_an_error_and_changes_nothing() {
    let dir = workspace();
    let edited = dir.join("kept.txt");
    fs::write(&edited, "original").unwrap();
    let server = server(
        None,
        Replies(vec![
            tool_reply("Write", &json!({"file_path": edited, "content": "new"})),
            text_reply("wrote it"),
        ]),
    )
    .await;

    let typed = "y\nchange it\ny\n/rewind abc\n/rewind 99\n/exit\n";
    let output = in_terminal(&server.uri(), &dir, NO_LOAD_ARGS, typed).await;

    assert_eq!(output.status.code(), Some(0), "{}", out(&output));
    assert!(
        out(&output).contains("Usage: /rewind <n>"),
        "{}",
        out(&output)
    );
    assert!(out(&output).contains("Rewind failed"), "{}", out(&output));
    assert_eq!(fs::read_to_string(&edited).unwrap(), "new");
}
