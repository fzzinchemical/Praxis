//! Gives the user a summary when a turn ends on tool calls with no final text, so small models
//! that stop silently still say what they did.

use super::recovery::parse_arguments;
use crate::provider::Message;

/// Summary of a turn's edits and commands, or `None` when the model wrote its own final text
/// or the turn made no edits or commands. Built from the turn's messages, from its last user prompt on.
pub fn fallback_summary(turn: &[Message]) -> Option<String> {
    let last = turn.iter().rev().find_map(|m| match m {
        Message::Assistant { content, .. } => Some(content),
        _ => None,
    })?;
    if last.as_deref().is_some_and(|text| !text.trim().is_empty()) {
        return None;
    }
    let mut edited: Vec<String> = Vec::new();
    let mut ran: Vec<String> = Vec::new();
    for message in turn {
        let Message::Assistant { tool_calls, .. } = message else {
            continue;
        };
        for call in tool_calls {
            let args = parse_arguments(&call.arguments);
            let arg = |key: &str| args.as_ref()?.get(key)?.as_str().map(str::to_owned);
            match call.name.as_str() {
                "Write" | "Edit" => {
                    if let Some(path) = arg("file_path").filter(|p| !edited.contains(p)) {
                        edited.push(path);
                    }
                }
                "Bash" => ran.extend(arg("command")),
                _ => {}
            }
        }
    }
    let mut parts = Vec::new();
    if !edited.is_empty() {
        parts.push(format!("Edited: {}", edited.join(", ")));
    }
    if !ran.is_empty() {
        parts.push(format!("Ran: {}", ran.join(", ")));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolCall;

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: "id".to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
        }
    }

    fn turn_with(calls: Vec<ToolCall>, final_text: Option<&str>) -> Vec<Message> {
        vec![
            Message::User {
                content: "fix it".to_owned(),
            },
            Message::Assistant {
                content: None,
                tool_calls: calls,
            },
            Message::Assistant {
                content: final_text.map(str::to_owned),
                tool_calls: Vec::new(),
            },
        ]
    }

    #[test]
    fn summarises_edits_and_commands_when_the_model_gives_no_final_text() {
        let turn = turn_with(
            vec![
                call("Edit", r#"{"file_path":"a.rs"}"#),
                call("Write", r#"{"file_path":"b.rs"}"#),
                call("Bash", r#"{"command":"cargo test"}"#),
            ],
            None,
        );

        assert_eq!(
            fallback_summary(&turn).as_deref(),
            Some("Edited: a.rs, b.rs · Ran: cargo test")
        );
    }

    #[test]
    fn leaves_a_non_empty_final_text_unchanged() {
        let turn = turn_with(
            vec![call("Bash", r#"{"command":"cargo test"}"#)],
            Some("All tests pass."),
        );

        assert_eq!(fallback_summary(&turn), None);
    }

    #[test]
    fn stays_silent_when_the_turn_made_no_edits_or_commands() {
        let turn = turn_with(vec![call("Read", r#"{"file_path":"a.rs"}"#)], None);

        assert_eq!(fallback_summary(&turn), None);
    }
}
