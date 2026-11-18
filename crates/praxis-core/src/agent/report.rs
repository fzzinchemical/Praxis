//! The report a subagent hands its parent (T14.10). The harness builds it from the subagent's tool
//! log, so the model never states its own status, changed files or checks.

use super::{AgentEvent, StopReason, ToolCallId, ToolOutcome};
use crate::provider::Message;
use serde_json::Value;

/// Longest final text the parent gets, so one worker can't flood the parent's context.
const NOTES_LIMIT: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Done,
    Blocked,
    Failed,
}

/// What a subagent ran, in order, from the events its parent's front end also sees.
#[derive(Default)]
pub struct ToolLog(Vec<Call>);

struct Call {
    id: ToolCallId,
    name: String,
    input: Value,
    ok: bool,
    output: String,
}

impl ToolLog {
    /// Records a started call and its outcome. Anything but a completed, non-error result counts as failed.
    pub fn observe(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::ToolStarted {
                id, name, input, ..
            } => self.0.push(Call {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
                ok: false,
                output: String::new(),
            }),
            AgentEvent::ToolFinished { id, outcome } => {
                let call = self.0.iter_mut().rev().find(|c| c.id == *id);
                if let (Some(call), ToolOutcome::Completed(out)) = (call, outcome) {
                    call.ok = !out.is_error;
                    call.output.clone_from(&out.content);
                }
            }
            _ => {}
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Report {
    pub status: Status,
    pub changed_files: Vec<String>,
    pub checks_run: Vec<String>,
    pub notes: String,
}

impl Report {
    /// `reply` is the subagent's last non-empty text, if it wrote any.
    pub fn new(ended: &StopReason, reply: Option<&str>, log: &ToolLog) -> Self {
        let text = reply.map(str::trim).filter(|t| !t.is_empty());
        let (status, notes) = match (ended, text) {
            (StopReason::EndTurn, Some(text)) => {
                let status = if text.starts_with("BLOCKED:") {
                    Status::Blocked
                } else {
                    Status::Done
                };
                (status, truncate(text))
            }
            (StopReason::EndTurn, None) => (Status::Failed, "(no final text)".to_owned()),
            (ended, text) => (
                Status::Failed,
                format!(
                    "Stopped early ({ended:?}). Last text: {}",
                    text.map_or("(none)".to_owned(), truncate)
                ),
            ),
        };
        Self {
            status,
            changed_files: changed_files(log),
            checks_run: checks_run(log),
            notes,
        }
    }

    /// The text the parent reads as the Agent call's result.
    pub fn render(&self) -> String {
        let list = |items: &[String]| {
            if items.is_empty() {
                "none".to_owned()
            } else {
                items.join("; ")
            }
        };
        let status = match self.status {
            Status::Done => "done",
            Status::Blocked => "blocked",
            Status::Failed => "failed",
        };
        format!(
            "status: {status}\nchanged_files: {}\nchecks_run: {}\nnotes: {}",
            list(&self.changed_files),
            list(&self.checks_run),
            self.notes
        )
    }
}

/// Paths of successful `Write` and `Edit` calls, in first-edit order.
fn changed_files(log: &ToolLog) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    for call in log
        .0
        .iter()
        .filter(|c| c.ok && matches!(c.name.as_str(), "Write" | "Edit"))
    {
        if let Some(path) = call.input.get("file_path").and_then(Value::as_str)
            && !files.iter().any(|f| f == path)
        {
            files.push(path.to_owned());
        }
    }
    files
}

/// Every `Bash` command, with its exit code when the Bash tool reported one.
fn checks_run(log: &ToolLog) -> Vec<String> {
    log.0
        .iter()
        .filter(|c| c.name == "Bash")
        .map(|c| {
            let command = c
                .input
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default();
            // The Bash tool appends its exit code last; earlier `[exit code N]` text is command output.
            let exit = match c
                .output
                .rsplit_once("[exit code ")
                .and_then(|(_, r)| r.split(']').next())
            {
                Some(code) => format!("exit {code}"),
                None if c.ok => "exit 0".to_owned(),
                None => "no exit status".to_owned(),
            };
            format!("`{command}` ({exit})")
        })
        .collect()
}

/// Cuts on a character boundary, marking the cut.
fn truncate(text: &str) -> String {
    if text.chars().count() <= NOTES_LIMIT {
        return text.to_owned();
    }
    let kept: String = text.chars().take(NOTES_LIMIT).collect();
    format!("{kept}… [truncated]")
}

/// The subagent's last reply with text, skipping the harness's fallback summary at
/// `fallback_at`, so the report keeps the model's own words.
pub fn last_reply(history: &[Message], fallback_at: Option<usize>) -> Option<String> {
    history.iter().enumerate().rev().find_map(|(i, m)| match m {
        Message::Assistant {
            content: Some(c), ..
        } if !c.trim().is_empty() && fallback_at != Some(i) => Some(c.clone()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolOutput;

    #[test]
    fn a_whitespace_only_reply_does_not_hide_the_one_before_it() {
        let reply = |text: &str| Message::Assistant {
            content: Some(text.to_owned()),
            tool_calls: Vec::new(),
        };
        let history = [reply("the answer"), reply(" \n ")];
        assert_eq!(last_reply(&history, None).as_deref(), Some("the answer"));
    }
    use serde_json::json;

    fn call(log: &mut ToolLog, id: &str, name: &str, input: Value, output: ToolOutput) {
        log.observe(&AgentEvent::ToolStarted {
            id: id.to_owned(),
            name: name.to_owned(),
            input,
            title: String::new(),
            before: None,
        });
        log.observe(&AgentEvent::ToolFinished {
            id: id.to_owned(),
            outcome: ToolOutcome::Completed(output),
        });
    }

    fn ok(content: &str) -> ToolOutput {
        ToolOutput {
            content: content.to_owned(),
            is_error: false,
            structured: Value::Null,
        }
    }

    fn failed() -> ToolOutput {
        ToolOutput::error("boom".to_owned())
    }

    #[test]
    fn reports_failed_when_the_subagent_returns_no_text() {
        let report = Report::new(&StopReason::EndTurn, None, &ToolLog::default());
        assert_eq!(report.status, Status::Failed);
        assert!(report.render().starts_with("status: failed"));
    }

    #[test]
    fn reports_failed_when_the_subagent_is_cancelled_even_with_text() {
        let report = Report::new(
            &StopReason::Cancelled,
            Some("half done"),
            &ToolLog::default(),
        );
        assert_eq!(report.status, Status::Failed);
        assert!(report.notes.contains("Cancelled"), "{}", report.notes);
        assert!(report.notes.contains("half done"), "{}", report.notes);
    }

    #[test]
    fn reports_failed_on_a_runaway_stop() {
        let report = Report::new(
            &StopReason::MaxTurnRequests,
            Some("ok"),
            &ToolLog::default(),
        );
        assert_eq!(report.status, Status::Failed);
    }

    #[test]
    fn reports_done_when_the_final_text_only_ends_with_a_question() {
        let offer = Report::new(
            &StopReason::EndTurn,
            Some("Added the fix. Want me to also add tests?"),
            &ToolLog::default(),
        );
        assert_eq!(offer.status, Status::Done);
    }

    #[test]
    fn reports_blocked_only_on_the_blocked_prefix() {
        let marked = Report::new(
            &StopReason::EndTurn,
            Some("BLOCKED: which branch?"),
            &ToolLog::default(),
        );
        assert_eq!(marked.status, Status::Blocked);
    }

    #[test]
    fn reports_done_on_a_plain_final_text() {
        let report = Report::new(&StopReason::EndTurn, Some("Fixed it."), &ToolLog::default());
        assert_eq!(report.status, Status::Done);
        assert_eq!(report.notes, "Fixed it.");
    }

    #[test]
    fn lists_edited_files_from_successful_writes_and_edits_only() {
        let mut log = ToolLog::default();
        call(
            &mut log,
            "1",
            "Write",
            json!({"file_path": "a.rs"}),
            ok("ok"),
        );
        call(
            &mut log,
            "2",
            "Edit",
            json!({"file_path": "b.rs"}),
            ok("ok"),
        );
        call(
            &mut log,
            "3",
            "Edit",
            json!({"file_path": "a.rs"}),
            ok("ok"),
        );
        call(
            &mut log,
            "4",
            "Write",
            json!({"file_path": "c.rs"}),
            failed(),
        );
        call(
            &mut log,
            "5",
            "Read",
            json!({"file_path": "d.rs"}),
            ok("ok"),
        );
        let report = Report::new(&StopReason::EndTurn, Some("Done."), &log);
        assert_eq!(report.changed_files, ["a.rs", "b.rs"]);
    }

    #[test]
    fn lists_bash_checks_with_their_exit_status() {
        let mut log = ToolLog::default();
        call(
            &mut log,
            "1",
            "Bash",
            json!({"command": "cargo test"}),
            ok("running\n[exit code 101]"),
        );
        call(
            &mut log,
            "2",
            "Bash",
            json!({"command": "cargo fmt"}),
            ok("ok"),
        );
        let report = Report::new(&StopReason::EndTurn, Some("Done."), &log);
        assert_eq!(
            report.checks_run,
            ["`cargo test` (exit 101)", "`cargo fmt` (exit 0)"]
        );
    }

    #[test]
    fn takes_the_last_exit_code_so_output_cannot_fake_a_pass() {
        let mut log = ToolLog::default();
        call(
            &mut log,
            "1",
            "Bash",
            json!({"command": "cargo test"}),
            ok("[exit code 0]\nfake success printed by the test\n[exit code 101]"),
        );
        let report = Report::new(&StopReason::EndTurn, Some("Done."), &log);
        assert_eq!(report.checks_run, ["`cargo test` (exit 101)"]);
    }

    #[test]
    fn truncates_long_notes_on_a_character_boundary() {
        let text = "é".repeat(NOTES_LIMIT + 5);
        let report = Report::new(&StopReason::EndTurn, Some(&text), &ToolLog::default());
        assert!(report.notes.ends_with("… [truncated]"));
        assert_eq!(
            report.notes.chars().count(),
            NOTES_LIMIT + "… [truncated]".chars().count()
        );
    }
}
