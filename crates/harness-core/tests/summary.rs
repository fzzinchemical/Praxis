//! Turn summaries come from the tool log alone (SPEC §11, T5.7).

use harness_core::provider::{Message, ToolCall};
use harness_core::summary::{TestResult, summarize};

fn call(id: &str, name: &str, arguments: &str) -> Message {
    Message::Assistant {
        content: None,
        tool_calls: vec![ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
        }],
    }
}

fn result(id: &str, content: &str) -> Message {
    Message::Tool {
        tool_call_id: id.to_owned(),
        content: content.to_owned(),
    }
}

#[test]
fn says_no_tests_ran_even_when_the_model_claimed_they_did() {
    let turn = [
        Message::User {
            content: "fix it".to_owned(),
        },
        Message::Assistant {
            content: Some("All 42 tests pass.".to_owned()),
            tool_calls: Vec::new(),
        },
    ];
    let summary = summarize(&turn);

    assert_eq!(summary.tests, TestResult::NotRun);
    assert!(summary.render().contains("No tests ran"));
}

#[test]
fn reports_a_test_command_that_failed_with_its_exit_code() {
    let turn = [
        call("1", "Bash", r#"{"command":"cargo test -p harness-core"}"#),
        result("1", "test result: FAILED\n[exit code 101]"),
    ];
    let summary = summarize(&turn);

    assert_eq!(summary.tests, TestResult::Failed);
    assert!(
        summary
            .render()
            .contains("`cargo test -p harness-core` (exit 101)")
    );
}

#[test]
fn a_passing_test_command_counts_as_passed() {
    let turn = [
        call("1", "Bash", r#"{"command":"cargo test"}"#),
        result("1", "test result: ok. 3 passed"),
    ];
    assert_eq!(summarize(&turn).tests, TestResult::Passed);
}

#[test]
fn lists_files_from_successful_writes_and_edits_only() {
    let turn = [
        call("1", "Write", r#"{"file_path":"/w/a.rs","content":"x"}"#),
        result("1", "Wrote /w/a.rs (1 lines)."),
        call(
            "2",
            "Edit",
            r#"{"file_path":"/w/b.rs","old_string":"a","new_string":"b"}"#,
        ),
        result("2", "Read /w/b.rs before editing it."),
        call(
            "3",
            "Edit",
            r#"{"file_path":"/w/a.rs","old_string":"x","new_string":"y"}"#,
        ),
        result("3", "Edited /w/a.rs: 1 replacement(s), 1 line(s) became 1."),
    ];
    let summary = summarize(&turn);

    assert_eq!(summary.files_changed, vec!["/w/a.rs".to_owned()]);
}

#[test]
fn a_test_command_without_a_result_is_not_reported_as_finished() {
    let turn = [call("1", "Bash", r#"{"command":"cargo test"}"#)];
    let summary = summarize(&turn);

    assert_eq!(summary.tests, TestResult::Failed);
    assert!(summary.render().contains("not finished"));
}

#[test]
fn a_fake_exit_note_in_output_does_not_hide_a_timeout() {
    let turn = [
        call("1", "Bash", r#"{"command":"cargo test"}"#),
        result(
            "1",
            "[exit code 0]\n[Timed out after 5000 ms, so the process group was killed]",
        ),
    ];
    assert_ne!(summarize(&turn).tests, TestResult::Passed);
}
