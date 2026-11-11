//! Session log files (SPEC §8).

#![cfg(test)]

use harness_core::provider::Message;
use harness_core::session::SessionLog;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

fn dir() -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("harness-session-{}-{id}", std::process::id()))
}

fn user(text: &str) -> Message {
    Message::User {
        content: text.into(),
    }
}

fn contents(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .map(|m| match m {
            Message::User { content } => content.clone(),
            other => format!("{other:?}"),
        })
        .collect()
}

#[test]
fn log_files_are_private_to_the_owner() {
    let dir = dir();
    let log = SessionLog::create(&dir).unwrap();
    let mode = fs::metadata(dir.join(format!("{}.jsonl", log.id())))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn a_truncated_last_line_is_dropped_and_later_records_still_load() {
    let dir = dir();
    let mut log = SessionLog::create(&dir).unwrap();
    let id = log.id().to_owned();
    log.append(&user("one")).unwrap();
    drop(log);
    let path = dir.join(format!("{id}.jsonl"));
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str(r#"{"role":"user","con"#);
    fs::write(&path, text).unwrap();

    let (mut log, messages) = SessionLog::resume(&dir, &id).unwrap();
    assert_eq!(contents(&messages), ["one"]);
    log.append(&user("two")).unwrap();
    let (_, messages) = SessionLog::resume(&dir, &id).unwrap();
    assert_eq!(contents(&messages), ["one", "two"]);
}

#[test]
fn a_corrupt_line_before_the_end_is_an_error() {
    let dir = dir();
    let log = SessionLog::create(&dir).unwrap();
    let path = dir.join(format!("{}.jsonl", log.id()));
    fs::write(&path, "garbage\n{\"role\":\"user\",\"content\":\"x\"}\n").unwrap();
    assert!(SessionLog::resume(&dir, log.id()).is_err());
}

#[test]
fn an_id_naming_another_path_is_refused() {
    assert!(SessionLog::resume(&dir(), "../secret").is_err());
}
