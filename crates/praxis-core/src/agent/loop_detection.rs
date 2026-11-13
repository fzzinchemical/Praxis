//! Stops a turn whose tool calls show the model is stuck (SPEC §5.4).

use super::recovery::parse_arguments;
use crate::provider::ToolCall;
use serde_json::Value;
use std::collections::{HashSet, VecDeque};

const IN_A_ROW: usize = 3;
const IN_WINDOW: usize = 5;
const WINDOW: usize = 20;
const IDLE_READS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LoopTrigger {
    RepeatedInARow,
    RepeatedInWindow,
    IdleReads,
}

impl LoopTrigger {
    pub(super) fn describe(self) -> &'static str {
        match self {
            Self::RepeatedInARow => "The same tool call ran 3 times in a row.",
            Self::RepeatedInWindow => "The same tool call ran 5 times within the last 20 calls.",
            Self::IdleReads => {
                "10 read-only calls in a row with no assistant text and nothing new read."
            }
        }
    }
}

/// Tracks one turn, so asking again in a new prompt starts clean.
#[derive(Default)]
pub(super) struct LoopDetector {
    recent: VecDeque<(String, Value)>,
    idle_reads: usize,
    ranges_read: HashSet<String>,
}

impl LoopDetector {
    /// `silent` says the assistant message holding this call had no text.
    pub(super) fn record(&mut self, call: &ToolCall, silent: bool) -> Option<LoopTrigger> {
        // Parsed so key order and spacing can't hide a repeat; broken JSON compares as text.
        let arguments = parse_arguments(&call.arguments)
            .unwrap_or_else(|| Value::String(call.arguments.clone()));

        // A range not seen before is progress: a large file can only be read in parts.
        // shortcut: paths compare as written, so `./a` and `a` are two files; normalize if models do this.
        let range = ["file_path", "offset", "limit"].map(|key| arguments.get(key).cloned());
        let new_range = call.name == "Read" && self.ranges_read.insert(format!("{range:?}"));
        let read_only = matches!(call.name.as_str(), "Read" | "Grep" | "Glob");
        self.idle_reads = match silent && read_only && !new_range {
            true => self.idle_reads + 1,
            false => 0,
        };

        if self.recent.len() == WINDOW {
            self.recent.pop_front();
        }
        self.recent.push_back((call.name.clone(), arguments));
        let latest = self.recent.back()?;
        let same = |entry: &&(String, Value)| *entry == latest;

        if self.recent.iter().rev().take_while(same).count() >= IN_A_ROW {
            return Some(LoopTrigger::RepeatedInARow);
        }
        if self.recent.iter().filter(same).count() >= IN_WINDOW {
            return Some(LoopTrigger::RepeatedInWindow);
        }
        (self.idle_reads >= IDLE_READS).then_some(LoopTrigger::IdleReads)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: "c".to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_string(),
        }
    }

    fn raw(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: "c".to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
        }
    }

    fn bash(command: &str) -> ToolCall {
        call("Bash", json!({ "command": command }))
    }

    fn grep(pattern: usize) -> ToolCall {
        call("Grep", json!({ "pattern": pattern.to_string() }))
    }

    fn read(path: &str, offset: usize) -> ToolCall {
        call("Read", json!({ "file_path": path, "offset": offset }))
    }

    /// Records every call and returns the 1-based position and kind of the first trigger.
    fn first_trigger(calls: &[ToolCall], silent: bool) -> Option<(usize, LoopTrigger)> {
        let mut detector = LoopDetector::default();
        calls
            .iter()
            .enumerate()
            .find_map(|(i, c)| Some((i + 1, detector.record(c, silent)?)))
    }

    #[test]
    fn the_same_call_three_times_in_a_row_triggers_on_the_third() {
        let calls = vec![bash("ls"); 3];
        assert_eq!(
            first_trigger(&calls, false),
            Some((3, LoopTrigger::RepeatedInARow))
        );
    }

    #[test]
    fn argument_key_order_does_not_hide_a_repeat() {
        let calls = [
            raw("Grep", r#"{"pattern":"a","path":"b"}"#),
            raw("Grep", r#"{"path":"b","pattern":"a"}"#),
            raw("Grep", r#"{ "pattern": "a", "path": "b" }"#),
        ];
        assert_eq!(
            first_trigger(&calls, false),
            Some((3, LoopTrigger::RepeatedInARow))
        );
    }

    #[test]
    fn repeated_invalid_arguments_trigger() {
        let calls = vec![raw("Read", "{not json"); 3];
        assert_eq!(
            first_trigger(&calls, false),
            Some((3, LoopTrigger::RepeatedInARow))
        );
    }

    #[test]
    fn the_same_arguments_on_another_tool_are_not_a_repeat() {
        let args = json!({ "pattern": "a" });
        let calls = [
            call("Grep", args.clone()),
            call("Glob", args.clone()),
            call("Grep", args),
        ];
        assert_eq!(first_trigger(&calls, false), None);
    }

    #[test]
    fn five_alternating_repeats_trigger_on_the_fifth() {
        let calls: Vec<ToolCall> = (0..9)
            .map(|i| match i % 2 {
                0 => bash("same"),
                _ => bash(&i.to_string()),
            })
            .collect();
        assert_eq!(
            first_trigger(&calls, false),
            Some((9, LoopTrigger::RepeatedInWindow))
        );
    }

    #[test]
    fn repeats_further_apart_than_the_window_do_not_trigger() {
        // The fifth repeat arrives at call 21, when the first has just left the last 20.
        let calls: Vec<ToolCall> = (0..21)
            .map(|i| match i % 5 {
                0 => bash("same"),
                _ => bash(&i.to_string()),
            })
            .collect();
        assert_eq!(first_trigger(&calls, false), None);
    }

    #[test]
    fn repeated_calls_with_different_arguments_do_not_trigger() {
        let edits: Vec<ToolCall> = (0..50)
            .map(|i| call("Edit", json!({ "file_path": "a.rs", "old_string": i })))
            .collect();
        assert_eq!(first_trigger(&edits, true), None);
        let reads: Vec<ToolCall> = (0..50).map(|i| read(&format!("{i}.rs"), 0)).collect();
        assert_eq!(first_trigger(&reads, true), None);
    }

    #[test]
    fn ten_silent_read_only_calls_trigger_on_the_tenth() {
        let calls: Vec<ToolCall> = (0..10).map(grep).collect();
        assert_eq!(
            first_trigger(&calls, true),
            Some((10, LoopTrigger::IdleReads))
        );
        assert_eq!(first_trigger(&calls[..9], true), None);
    }

    #[test]
    fn reading_new_ranges_of_one_file_is_progress() {
        let calls: Vec<ToolCall> = (0..50).map(|i| read("a.rs", i)).collect();
        assert_eq!(first_trigger(&calls, true), None);
    }

    #[test]
    fn rereading_a_known_range_counts_as_idle() {
        // Three new ranges, then 10 idle calls: new searches and rereads of those ranges.
        let mut calls: Vec<ToolCall> = (0..3).map(|i| read("a.rs", i)).collect();
        calls.extend((0..7).map(grep));
        calls.extend((0..3).map(|i| read("a.rs", i)));
        assert_eq!(
            first_trigger(&calls, true),
            Some((13, LoopTrigger::IdleReads))
        );
        assert_eq!(first_trigger(&calls[..12], true), None);
    }

    #[test]
    fn assistant_text_resets_the_idle_count() {
        let mut detector = LoopDetector::default();
        for i in 0..9 {
            assert_eq!(detector.record(&grep(i), true), None);
        }
        assert_eq!(detector.record(&grep(9), false), None);
        for i in 10..19 {
            assert_eq!(detector.record(&grep(i), true), None);
        }
    }

    #[test]
    fn a_new_file_resets_the_idle_count() {
        let mut calls: Vec<ToolCall> = (0..9).map(grep).collect();
        calls.push(read("new.rs", 0));
        calls.extend((9..18).map(grep));
        assert_eq!(first_trigger(&calls, true), None);
    }

    #[test]
    fn a_call_that_is_not_read_only_resets_the_idle_count() {
        let mut calls: Vec<ToolCall> = (0..9).map(grep).collect();
        calls.push(bash("cargo test"));
        calls.extend((9..18).map(grep));
        assert_eq!(first_trigger(&calls, true), None);
    }
}
