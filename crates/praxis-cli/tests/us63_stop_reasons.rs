//! US-63: the user is told why a turn stopped, and the exit code says so too.

#![cfg(test)]

// Each test binary uses only part of the shared helpers.
#[allow(dead_code)]
mod common;
use common::*;

use serde_json::json;
use std::fs;

use std::time::Duration;
use tokio::process::{Child, Command};

fn cut_off_reply(text: &str) -> String {
    sse(&[
        json!({"choices": [{"delta": {"content": text}, "finish_reason": null}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "length"}]}),
    ])
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_cut_off_at_the_output_limit_says_so_and_exits_1() {
    let server = server(Some(32768), Replies(vec![cut_off_reply("partial answ")])).await;

    let output = run(&server, &workspace(), &["--model", "m", "hi"]).await;

    assert_eq!(output.status.code(), Some(1), "{}", err(&output));
    assert!(
        err(&output).contains("The model hit its output limit"),
        "{}",
        err(&output)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_stopped_by_the_request_limit_says_so_and_exits_1() {
    let dir = workspace();
    fs::create_dir_all(dir.join("xdg/praxis")).unwrap();
    fs::write(
        dir.join("xdg/praxis/config.toml"),
        "[agent]\nmax_turn_requests = 1\n",
    )
    .unwrap();
    fs::write(dir.join("notes.txt"), "hello\n").unwrap();
    let server = server(
        Some(32768),
        Replies(vec![
            tool_reply("Read", &json!({"file_path": dir.join("notes.txt")})),
            text_reply("done"),
        ]),
    )
    .await;

    let output = run(&server, &dir, &["--model", "m", "read notes"]).await;

    assert_eq!(output.status.code(), Some(1), "{}", err(&output));
    assert!(
        err(&output).contains("Stopped at the request limit"),
        "{}",
        err(&output)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_turn_exits_130() {
    let server = server(Some(32768), Hang).await;
    let child: Child = command(&server.uri(), &workspace(), &["--model", "m", "hi"])
        .spawn()
        .unwrap();
    let chat_arrived = async {
        while requests_to(&server, CHAT).await.is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), chat_arrived)
        .await
        .unwrap();

    let pid = child.id().unwrap().to_string();
    let killed = Command::new("kill").args(["-INT", &pid]).status();
    assert!(killed.await.unwrap().success());
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(output.status.code(), Some(130), "{}", err(&output));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_normal_reply_exits_0_and_prints_no_limit_message() {
    let server = server(Some(32768), Replies(vec![text_reply("all good")])).await;

    let output = run(&server, &workspace(), &["--model", "m", "hi"]).await;

    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    assert!(!err(&output).contains("output limit"), "{}", err(&output));
    assert!(!err(&output).contains("request limit"), "{}", err(&output));
    assert!(out(&output).contains("all good"), "{}", out(&output));
}
