//! Turn summaries built from the tool log alone. Model text is ignored on purpose: the model
//! may claim tests passed when none ran (SPEC §11).

use crate::provider::Message;
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestResult {
    NotRun,
    Passed,
    Failed,
}

#[derive(Debug, PartialEq, Eq)]
pub struct CommandRun {
    pub command: String,
    /// `exit <code>`, or `not finished` when there is no exit code (cancel, timeout, signal).
    pub status: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TurnSummary {
    pub files_changed: Vec<String>,
    pub commands: Vec<CommandRun>,
    pub tests: TestResult,
}

/// The fields this module reads. Other fields of a tool's input are ignored.
#[derive(Deserialize)]
struct Args {
    command: Option<String>,
    file_path: Option<String>,
}

// shortcut: matches runner names by substring, so wrappers such as `make test` are missed; add them as seen.
const TEST_RUNNERS: &[&str] = &[
    "cargo test",
    "cargo nextest",
    "pytest",
    "npm test",
    "npm run test",
    "scripts/verify",
];

/// Summarises a turn's messages, the slice after its user prompt.
pub fn summarize(turn: &[Message]) -> TurnSummary {
    let results: HashMap<&str, &str> = turn
        .iter()
        .filter_map(|message| match message {
            Message::Tool {
                tool_call_id,
                content,
            } => Some((tool_call_id.as_str(), content.as_str())),
            _ => None,
        })
        .collect();
    let mut summary = TurnSummary {
        files_changed: Vec::new(),
        commands: Vec::new(),
        tests: TestResult::NotRun,
    };
    for message in turn {
        let Message::Assistant { tool_calls, .. } = message else {
            continue;
        };
        for call in tool_calls {
            let Ok(args) = serde_json::from_str::<Args>(&call.arguments) else {
                continue;
            };
            let outcome = results.get(call.id.as_str()).copied();
            match (call.name.as_str(), args) {
                (
                    "Bash",
                    Args {
                        command: Some(command),
                        ..
                    },
                ) => {
                    let code = exit_code(outcome);
                    if TEST_RUNNERS.iter().any(|runner| command.contains(runner)) {
                        let passed = code == Some(0) && summary.tests != TestResult::Failed;
                        summary.tests = if passed {
                            TestResult::Passed
                        } else {
                            TestResult::Failed
                        };
                    }
                    let status = match code {
                        Some(code) => format!("exit {code}"),
                        None => "not finished".to_owned(),
                    };
                    summary.commands.push(CommandRun { command, status });
                }
                (
                    "Write",
                    Args {
                        file_path: Some(path),
                        ..
                    },
                ) if outcome.is_some_and(|r| r.starts_with("Wrote ")) => {
                    push_unique(&mut summary.files_changed, path);
                }
                (
                    "Edit",
                    Args {
                        file_path: Some(path),
                        ..
                    },
                ) if outcome.is_some_and(|r| r.starts_with("Edited ")) => {
                    push_unique(&mut summary.files_changed, path);
                }
                _ => {}
            }
        }
    }
    summary
}

fn push_unique(files: &mut Vec<String>, path: String) {
    if !files.contains(&path) {
        files.push(path);
    }
}

/// Reads the exit code from the trailing note the Bash tool adds. `None` means the command
/// never finished: no result, a timeout, a cancel, a signal or a failed wait.
fn exit_code(result: Option<&str>) -> Option<i32> {
    let result = result?;
    if let Some((_, rest)) = result.rsplit_once("[exit code ") {
        return rest.split(']').next()?.parse().ok();
    }
    let unfinished = [
        "[killed by a signal]",
        "[Timed out",
        "[Cancelled",
        "[couldn't wait",
    ];
    if unfinished.iter().any(|note| result.contains(note)) {
        return None;
    }
    Some(0)
}

impl TurnSummary {
    /// Plain text for the developer. It states when no tests ran.
    pub fn render(&self) -> String {
        let files = if self.files_changed.is_empty() {
            "none".to_owned()
        } else {
            self.files_changed.join(", ")
        };
        let commands = if self.commands.is_empty() {
            "none".to_owned()
        } else {
            self.commands
                .iter()
                .map(|run| format!("`{}` ({})", run.command, run.status))
                .collect::<Vec<_>>()
                .join("; ")
        };
        let tests = match self.tests {
            TestResult::NotRun => "No tests ran this turn.",
            TestResult::Passed => "Tests passed.",
            TestResult::Failed => "Tests failed or did not finish.",
        };
        format!("Files changed: {files}\nCommands run: {commands}\n{tests}")
    }
}
