//! Session log files (SPEC §8).

#![cfg(test)]

use praxis_core::provider::Message;
use praxis_core::session::{self, SessionLog};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn dir() -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("praxis-session-{}-{id}", std::process::id()))
}

/// An existing directory to use as a workspace.
fn workspace() -> PathBuf {
    let ws = dir();
    fs::create_dir_all(&ws).unwrap();
    ws
}

/// Creates a session in `workspace` holding one user prompt and returns its id.
fn log_in(sessions: &Path, workspace: &Path, prompt: &str) -> String {
    let mut log = SessionLog::create(sessions, workspace).unwrap();
    log.append(&user(prompt)).unwrap();
    log.id().to_owned()
}

fn set_last_active(sessions: &Path, id: &str, secs: u64) {
    let path = sessions.join(format!("{id}.jsonl"));
    let file = fs::File::options().write(true).open(path).unwrap();
    file.set_modified(UNIX_EPOCH + Duration::from_secs(secs))
        .unwrap();
}

fn ids(sessions: &Path, workspace: &Path) -> Vec<String> {
    session::list(sessions, workspace)
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect()
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
    let log = SessionLog::create(&dir, &workspace()).unwrap();
    let mode = fs::metadata(dir.join(format!("{}.jsonl", log.id())))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn a_truncated_last_line_is_dropped_and_later_records_still_load() {
    let dir = dir();
    let mut log = SessionLog::create(&dir, &workspace()).unwrap();
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
    drop(log);
    let (_, messages) = SessionLog::resume(&dir, &id).unwrap();
    assert_eq!(contents(&messages), ["one", "two"]);
}

#[test]
fn a_corrupt_line_before_the_end_is_an_error() {
    let dir = dir();
    let log = SessionLog::create(&dir, &workspace()).unwrap();
    let id = log.id().to_owned();
    drop(log);
    let path = dir.join(format!("{id}.jsonl"));
    fs::write(&path, "garbage\n{\"role\":\"user\",\"content\":\"x\"}\n").unwrap();
    assert!(SessionLog::resume(&dir, &id).is_err());
}

#[test]
fn an_id_naming_another_path_is_refused() {
    assert!(SessionLog::resume(&dir(), "../secret").is_err());
}

#[test]
fn listing_keeps_only_the_given_workspace_by_canonical_path() {
    let mine = workspace();
    let link = dir();
    std::os::unix::fs::symlink(&mine, &link).unwrap();
    let sessions = dir();
    let here = log_in(&sessions, &mine, "here");
    log_in(&sessions, &workspace(), "elsewhere");

    assert_eq!(ids(&sessions, &link), [here]);
}

#[test]
fn listing_orders_by_last_activity_newest_first() {
    let ws = workspace();
    let sessions = dir();
    let older = log_in(&sessions, &ws, "older");
    let newer = log_in(&sessions, &ws, "newer");
    set_last_active(&sessions, &older, 2_000);
    set_last_active(&sessions, &newer, 1_000);

    assert_eq!(ids(&sessions, &ws), [older, newer]);
}

#[test]
fn listing_skips_headerless_and_truncated_logs() {
    let ws = workspace();
    let sessions = dir();
    fs::create_dir_all(&sessions).unwrap();
    let kept = log_in(&sessions, &ws, "kept");
    fs::write(
        sessions.join("headerless.jsonl"),
        "{\"role\":\"user\",\"content\":\"old\"}\n",
    )
    .unwrap();
    fs::write(
        sessions.join("truncated.jsonl"),
        r#"{"session":{"workspace":"#,
    )
    .unwrap();

    assert_eq!(ids(&sessions, &ws), [kept]);
}

#[test]
fn listing_reports_the_trimmed_first_prompt_and_start_time() {
    let ws = workspace();
    let sessions = dir();
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut log = SessionLog::create(&sessions, &ws).unwrap();
    log.append(&user("  fix the parser \n")).unwrap();
    log.append(&user("later")).unwrap();
    drop(log);

    let info = session::list(&sessions, &ws).unwrap().remove(0);

    assert_eq!(info.title, "fix the parser");
    assert!(info.started >= before && info.started <= info.last_active);
}

#[test]
fn a_session_open_elsewhere_cannot_be_resumed_again() {
    let ws = workspace();
    let sessions = dir();
    let id = log_in(&sessions, &ws, "hi");
    let (first, _) = SessionLog::resume(&sessions, &id).unwrap();

    let Err(err) = SessionLog::resume(&sessions, &id) else {
        panic!("a second resume must fail while the first log is alive");
    };
    assert!(
        err.to_string().contains("is open in another window"),
        "unexpected error: {err}"
    );

    drop(first);
    assert!(SessionLog::resume(&sessions, &id).is_ok());
}

#[test]
fn resuming_does_not_replay_the_header() {
    let ws = workspace();
    let sessions = dir();
    let id = log_in(&sessions, &ws, "hi");

    let (_, messages) = SessionLog::resume(&sessions, &id).unwrap();

    assert_eq!(contents(&messages), ["hi"]);
}

#[test]
fn deleting_a_session_removes_only_its_log() {
    let ws = workspace();
    let sessions = dir();
    let gone = log_in(&sessions, &ws, "delete me");
    let kept = log_in(&sessions, &ws, "keep me");

    session::delete(&sessions, &gone).unwrap();

    assert_eq!(ids(&sessions, &ws), vec![kept]);
}

#[test]
fn deleting_an_id_naming_another_path_removes_nothing_outside() {
    let sessions = dir();
    fs::create_dir_all(&sessions).unwrap();
    let name = format!("victim-{}", std::process::id());
    let outside = sessions.parent().unwrap().join(format!("{name}.jsonl"));
    fs::write(&outside, "keep").unwrap();

    assert!(session::delete(&sessions, &format!("../{name}")).is_err());
    assert!(session::delete(&sessions, "").is_err());
    assert!(session::delete(&sessions, "a/b").is_err());

    assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
    fs::remove_file(&outside).unwrap();
}

/// The session's folder as `list` reports it, read fresh from the files on disk.
fn folder_of(sessions: &Path, workspace: &Path, id: &str) -> Option<String> {
    session::list(sessions, workspace)
        .unwrap()
        .into_iter()
        .find(|s| s.id == id)
        .and_then(|s| s.folder)
}

#[test]
fn a_session_moved_to_a_folder_lists_under_it_and_keeps_it_after_a_restart() {
    let ws = workspace();
    let sessions = dir();
    let filed = log_in(&sessions, &ws, "file me");
    let unfiled = log_in(&sessions, &ws, "leave me");

    session::set_folder(&sessions, &filed, "  Work  ").unwrap();

    assert_eq!(folder_of(&sessions, &ws, &filed), Some("Work".to_owned()));
    assert_eq!(folder_of(&sessions, &ws, &unfiled), None);
}

#[test]
fn an_empty_folder_name_removes_the_session_from_its_folder() {
    let ws = workspace();
    let sessions = dir();
    let id = log_in(&sessions, &ws, "move me back");
    session::set_folder(&sessions, &id, "Work").unwrap();

    session::set_folder(&sessions, &id, "   ").unwrap();

    assert_eq!(folder_of(&sessions, &ws, &id), None);
}

#[test]
fn folder_names_are_limited_to_64_printable_characters() {
    let ws = workspace();
    let sessions = dir();
    let id = log_in(&sessions, &ws, "name me");

    assert!(session::set_folder(&sessions, &id, &"x".repeat(65)).is_err());
    assert!(session::set_folder(&sessions, &id, "bad\nname").is_err());
    assert!(session::set_folder(&sessions, &id, &"x".repeat(64)).is_ok());
    assert_eq!(folder_of(&sessions, &ws, &id), Some("x".repeat(64)));
}

#[test]
fn a_corrupt_folder_file_reads_as_no_folders() {
    let ws = workspace();
    let sessions = dir();
    let id = log_in(&sessions, &ws, "still listed");
    fs::write(sessions.join("folders.json"), "{ not json").unwrap();

    assert_eq!(ids(&sessions, &ws), vec![id.clone()]);
    assert_eq!(folder_of(&sessions, &ws, &id), None);
}

#[test]
fn deleting_a_session_removes_its_folder_entry() {
    let ws = workspace();
    let sessions = dir();
    let gone = log_in(&sessions, &ws, "delete me");
    let kept = log_in(&sessions, &ws, "keep me");
    session::set_folder(&sessions, &gone, "Scratch").unwrap();
    session::set_folder(&sessions, &kept, "Work").unwrap();

    session::delete(&sessions, &gone).unwrap();

    let folders = fs::read_to_string(sessions.join("folders.json")).unwrap();
    assert!(!folders.contains(&gone));
    assert_eq!(folder_of(&sessions, &ws, &kept), Some("Work".to_owned()));
}

#[test]
fn setting_a_folder_refuses_ids_outside_the_session_path_rules() {
    let sessions = dir();
    fs::create_dir_all(&sessions).unwrap();

    assert!(session::set_folder(&sessions, "../folders", "Work").is_err());
    assert!(session::set_folder(&sessions, "", "Work").is_err());
    assert!(!sessions.join("folders.json").exists());
}

#[test]
fn a_subagent_entry_stays_out_of_the_main_history_but_keeps_its_parent() {
    let dir = dir();
    let mut log = SessionLog::create(&dir, &workspace()).unwrap();
    let id = log.id().to_owned();
    log.append(&user("go")).unwrap();
    log.append_subagent("c0", &user("inner")).unwrap();
    drop(log);
    let (_, entries) = SessionLog::resume_entries(&dir, &id).unwrap();
    let parents: Vec<Option<&str>> = entries.iter().map(|e| e.subagent_of.as_deref()).collect();
    assert_eq!(parents, [None, Some("c0")]);
    let (_, messages) = SessionLog::resume(&dir, &id).unwrap();
    assert_eq!(contents(&messages), ["go"]);
}

#[test]
fn a_log_line_written_before_the_subagent_field_still_loads() {
    let dir = dir();
    let log = SessionLog::create(&dir, &workspace()).unwrap();
    let id = log.id().to_owned();
    drop(log);
    let path = dir.join(format!("{id}.jsonl"));
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str("{\"role\":\"user\",\"content\":\"old\"}\n");
    fs::write(&path, text).unwrap();
    let (_, entries) = SessionLog::resume_entries(&dir, &id).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].subagent_of, None);
}
