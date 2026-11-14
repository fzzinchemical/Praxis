//! Turns weak models' output into tool calls (SPEC §5.3). Everything here is about structure:
//! a recovered call still goes through schema validation and the permission path.

use crate::provider::{ToolCall, ToolSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How a model expresses tool calls. Text formats are parsed only from the assistant's own
/// reply, never from tool results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolFormat {
    Native,
    /// `<tool_call>{json}</tool_call>`.
    HermesText,
    /// A code fence holding `<tools>` or `<response>`, as qwen2.5-coder writes (ADR 0002).
    FencedJson,
}

impl ToolFormat {
    /// The system prompt addition teaching a text format, or `None` for native calling.
    /// `tools` is listed in a fixed order so the prompt prefix stays cacheable (SPEC §7).
    pub fn instructions(self, tools: &[ToolSchema]) -> Option<String> {
        let example = r#"{"name": "<tool>", "arguments": {<arguments>}}"#;
        let shape = match self {
            Self::Native => return None,
            Self::HermesText => format!("<tool_call>{example}</tool_call>"),
            Self::FencedJson => format!("```xml\n<tools>\n{example}\n</tools>\n```"),
        };
        let listing: Vec<String> = tools
            .iter()
            .map(|t| {
                format!(
                    "- {}: {} Arguments: {}",
                    t.name, t.description, t.parameters
                )
            })
            .collect();
        Some(format!(
            "\n\nTo call a tool, write exactly this, with valid JSON:\n{shape}\nTools:\n{}",
            listing.join("\n")
        ))
    }
}

/// The name given to a text-embedded call whose body can't be read, so the model hears about it.
const UNREADABLE_CALL: &str = "unreadable_tool_call";
/// Enough for a string holding a string holding the arguments; bounds hostile nesting.
const MAX_ENCODING_DEPTH: u32 = 3;

#[derive(Debug, PartialEq, Eq)]
pub struct Recovered {
    /// The reply with any call markup removed.
    pub text: String,
    pub calls: Vec<ToolCall>,
}

/// Picks the calls in a finished reply. Native calls win and text is then left alone.
/// `id_prefix` keeps generated call ids unique within a conversation.
pub fn recover_calls(
    text: &str,
    native: Vec<ToolCall>,
    format: ToolFormat,
    id_prefix: &str,
) -> Recovered {
    let (text, bodies) = match format {
        _ if !native.is_empty() => (text.to_owned(), Vec::new()),
        ToolFormat::Native => (text.to_owned(), Vec::new()),
        ToolFormat::HermesText => hermes_bodies(text),
        ToolFormat::FencedJson => fenced_bodies(text),
    };
    let mut calls = native;
    for call in &mut calls {
        if let Some(input) = parse_arguments(&call.arguments) {
            call.arguments = input.to_string();
        }
    }
    let parsed = bodies.iter().flat_map(|body| calls_in_body(body));
    calls.extend(parsed.enumerate().map(|(i, (name, arguments))| ToolCall {
        id: format!("{id_prefix}-{i}"),
        name,
        arguments,
    }));
    Recovered { text, calls }
}

/// Model arguments are untrusted text: anything that isn't a JSON object after repair is `None`.
pub fn parse_arguments(arguments: &str) -> Option<Value> {
    let mut value = repair_json(arguments)?;
    for _ in 0..MAX_ENCODING_DEPTH {
        match value {
            Value::String(inner) => value = repair_json(&inner)?,
            _ => break,
        }
    }
    value.is_object().then_some(value)
}

fn hermes_bodies(text: &str) -> (String, Vec<String>) {
    const OPEN: &str = "<tool_call>";
    const CLOSE: &str = "</tool_call>";
    let (mut kept, mut bodies, mut rest) = (String::new(), Vec::new(), text);
    while let Some((before, after)) = rest.split_once(OPEN) {
        // A call cut off by the output limit has no closing tag, and stays text.
        let Some((body, tail)) = after.split_once(CLOSE) else {
            break;
        };
        kept.push_str(before);
        bodies.push(body.to_owned());
        rest = tail;
    }
    kept.push_str(rest);
    (kept, bodies)
}

fn fenced_bodies(text: &str) -> (String, Vec<String>) {
    const FENCE: &str = "```";
    let (mut kept, mut bodies, mut rest) = (String::new(), Vec::new(), text);
    while let Some((before, after)) = rest.split_once(FENCE) {
        let Some((fenced, tail)) = after.split_once(FENCE) else {
            break;
        };
        kept.push_str(before);
        match tagged_body(fenced) {
            Some(body) => bodies.push(body.to_owned()),
            None => {
                kept.push_str(FENCE);
                kept.push_str(fenced);
                kept.push_str(FENCE);
            }
        }
        rest = tail;
    }
    kept.push_str(rest);
    (kept, bodies)
}

fn tagged_body(fenced: &str) -> Option<&str> {
    let (_language, inner) = fenced.split_once('\n')?;
    let inner = inner.trim();
    ["tools", "response"].iter().find_map(|tag| {
        inner
            .strip_prefix(&format!("<{tag}>"))?
            .strip_suffix(&format!("</{tag}>"))
    })
}

/// A body holds one object, an array, or objects in sequence. Each yields `(name, arguments)`.
fn calls_in_body(body: &str) -> Vec<(String, String)> {
    let objects: Vec<Value> = match repair_json(body) {
        Some(Value::Array(items)) => items,
        Some(value) => vec![value],
        None => sequence(body),
    };
    if objects.is_empty() {
        return vec![(UNREADABLE_CALL.to_owned(), body.trim().to_owned())];
    }
    objects
        .iter()
        .map(
            |object| match (object["name"].as_str(), &object["arguments"]) {
                (Some(name), Value::String(arguments)) => (
                    name.to_owned(),
                    parse_arguments(arguments).map_or_else(|| arguments.clone(), |v| v.to_string()),
                ),
                (Some(name), arguments) => (name.to_owned(), arguments.to_string()),
                _ => (UNREADABLE_CALL.to_owned(), object.to_string()),
            },
        )
        .collect()
}

/// Objects one after another, whether strict and pretty-printed or one per line needing repair.
fn sequence(body: &str) -> Vec<Value> {
    let strict: Result<Vec<Value>, _> = serde_json::Deserializer::from_str(body)
        .into_iter()
        .collect();
    strict.unwrap_or_else(|_| {
        let lines: Option<Vec<Value>> = body
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(repair_json)
            .collect();
        lines.unwrap_or_default()
    })
}

/// Repairs only what strict parsing rejects: valid JSON is never altered.
fn repair_json(text: &str) -> Option<Value> {
    if let Ok(value) = serde_json::from_str(text) {
        return Some(value);
    }
    let cleaned = normalize_tokens(strip_code_fence(text.trim()));
    let mut candidate = cleaned.trim();
    loop {
        if let Ok(value) = serde_json::from_str(candidate) {
            return Some(value);
        }
        // shortcut: only outer doubled braces unwrap, all the recordings show; handle inner ones if a model emits them.
        if !(candidate.starts_with("{{") && candidate.ends_with("}}")) {
            return None;
        }
        candidate = &candidate[1..candidate.len() - 1];
    }
}

fn strip_code_fence(text: &str) -> &str {
    let inner = text
        .strip_prefix("```")
        .and_then(|rest| rest.strip_suffix("```"))
        .and_then(|rest| rest.split_once('\n'));
    inner.map_or(text, |(_language, body)| body)
}

/// Rewrites to strict JSON: single-quoted strings, literal control characters inside strings,
/// and trailing commas.
fn normalize_tokens(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut quote = None;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, '"' | '\'') => {
                quote = Some(c);
                out.push('"');
            }
            (None, '}' | ']') => {
                let end = out.trim_end().len();
                if out[..end].ends_with(',') {
                    out.truncate(end - 1);
                }
                out.push(c);
            }
            (None, _) => out.push(c),
            (Some(open), _) if c == open => {
                quote = None;
                out.push('"');
            }
            (Some(_), '\\') => match chars.next() {
                Some('\'') => out.push('\''),
                Some(escaped) => {
                    out.push('\\');
                    out.push(escaped);
                }
                None => out.push('\\'),
            },
            (Some(_), '"') => out.push_str("\\\""),
            (Some(_), '\n') => out.push_str("\\n"),
            (Some(_), '\r') => out.push_str("\\r"),
            (Some(_), '\t') => out.push_str("\\t"),
            (Some(_), _) => out.push(c),
        }
    }
    out
}

/// Splits `<think>` blocks out of streamed text. Only a block opening the reply counts, so a
/// reply discussing the tag stays intact.
#[derive(Debug, Default)]
pub struct ThinkSplitter {
    state: ThinkState,
    held: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
enum ThinkState {
    #[default]
    Start,
    Inside,
    Text,
}

/// What one piece of stream contributes.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Split {
    pub thinking: String,
    pub text: String,
}

impl ThinkSplitter {
    const OPEN: &str = "<think>";
    const CLOSE: &str = "</think>";

    pub fn push(&mut self, delta: &str) -> Split {
        let mut split = Split::default();
        if self.state == ThinkState::Text {
            split.text.push_str(delta);
            return split;
        }
        self.held.push_str(delta);
        if self.state == ThinkState::Start {
            let trimmed = self.held.trim_start();
            if let Some(rest) = trimmed.strip_prefix(Self::OPEN) {
                self.held = rest.to_owned();
                self.state = ThinkState::Inside;
            } else if Self::OPEN.starts_with(trimmed) {
                return split;
            } else {
                self.state = ThinkState::Text;
                split.text = std::mem::take(&mut self.held);
                return split;
            }
        }
        if let Some((thinking, after)) = self.held.split_once(Self::CLOSE) {
            split.thinking = thinking.to_owned();
            split.text = after.to_owned();
            self.held.clear();
            self.state = ThinkState::Text;
            return split;
        }
        // Hold back any tail that could be the start of the closing tag.
        let keep = (1..Self::CLOSE.len())
            .rev()
            .find(|&n| self.held.ends_with(&Self::CLOSE[..n]))
            .unwrap_or(0);
        let cut = self.held.len() - keep;
        split.thinking = self.held[..cut].to_owned();
        self.held.drain(..cut);
        split
    }

    /// Call when the reply ends: whatever was held back is final.
    pub fn finish(&mut self) -> Split {
        let held = std::mem::take(&mut self.held);
        match std::mem::replace(&mut self.state, ThinkState::Text) {
            ThinkState::Inside => Split {
                thinking: held,
                text: String::new(),
            },
            _ => Split {
                thinking: String::new(),
                text: held,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn native(arguments: &str) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: "Read".into(),
            arguments: arguments.into(),
        }
    }

    #[test]
    fn repairs_each_malformation_named_in_the_spec() {
        let want = json!({"a": "b"});
        for broken in [
            r#"{"a": "b",}"#,
            "{'a': 'b'}",
            "{{\"a\": \"b\"}}",
            "```json\n{\"a\": \"b\"}\n```",
            r#""{\"a\": \"b\"}""#,
        ] {
            assert_eq!(parse_arguments(broken).as_ref(), Some(&want), "{broken}");
        }
        assert_eq!(
            parse_arguments("{\"a\": \"line\nbreak\"}"),
            Some(json!({"a": "line\nbreak"}))
        );
    }

    #[test]
    fn valid_json_with_awkward_content_is_left_exactly_as_written() {
        let want = json!({"c": "it's, {{x}}, [1,] ```"});
        assert_eq!(parse_arguments(&want.to_string()), Some(want));
    }

    #[test]
    fn a_single_quoted_string_keeps_its_quotes_and_apostrophes() {
        assert_eq!(
            parse_arguments(r#"{'a': 'say "hi" and it\'s'}"#),
            Some(json!({"a": "say \"hi\" and it's"}))
        );
    }

    #[test]
    fn hopeless_or_non_object_arguments_are_rejected() {
        for bad in ["", "not json", "[1]", "3", r#""hello""#, "{\"a\":"] {
            assert_eq!(parse_arguments(bad), None, "{bad}");
        }
    }

    #[test]
    fn encoding_depth_is_bounded() {
        let mut nested = json!({"a": 1}).to_string();
        for _ in 0..10 {
            nested = Value::String(nested).to_string();
        }
        assert_eq!(parse_arguments(&nested), None);
    }

    #[test]
    fn native_calls_win_over_any_text_call() {
        let text = "<tool_call>{\"name\":\"Bash\",\"arguments\":{}}</tool_call>";
        let got = recover_calls(text, vec![native("{}")], ToolFormat::HermesText, "t");
        assert_eq!(got.calls.len(), 1);
        assert_eq!(got.calls[0].name, "Read");
        assert_eq!(got.text, text);
    }

    #[test]
    fn hermes_calls_are_lifted_out_of_the_surrounding_text() {
        let text = "Reading.\n<tool_call>{\"name\":\"Read\",\"arguments\":\"{\\\"file_path\\\":\\\"a\\\"}\"}</tool_call>";
        let got = recover_calls(text, Vec::new(), ToolFormat::HermesText, "t");
        assert_eq!(got.text, "Reading.\n");
        assert_eq!(got.calls[0].id, "t-0");
        assert_eq!(got.calls[0].arguments, r#"{"file_path":"a"}"#);
    }

    #[test]
    fn an_unclosed_call_stays_text() {
        let text = "<tool_call>{\"name\":\"Read\"";
        let got = recover_calls(text, Vec::new(), ToolFormat::HermesText, "t");
        assert!(got.calls.is_empty());
        assert_eq!(got.text, text);
    }

    #[test]
    fn an_unreadable_body_becomes_a_call_that_can_fail_validation() {
        let text = "<tool_call>not json</tool_call>";
        let got = recover_calls(text, Vec::new(), ToolFormat::HermesText, "t");
        assert_eq!(got.calls[0].name, UNREADABLE_CALL);
    }

    #[test]
    fn ordinary_code_fences_are_kept() {
        let text = "See:\n```rust\nfn main() {}\n```\nDone.";
        let got = recover_calls(text, Vec::new(), ToolFormat::FencedJson, "t");
        assert!(got.calls.is_empty());
        assert_eq!(got.text, text);
    }

    fn split_all(pieces: &[&str]) -> Split {
        let mut splitter = ThinkSplitter::default();
        let mut all = Split::default();
        let mut add = |part: Split| {
            all.thinking.push_str(&part.thinking);
            all.text.push_str(&part.text);
        };
        for piece in pieces {
            add(splitter.push(piece));
        }
        add(splitter.finish());
        all
    }

    #[test]
    fn think_blocks_split_correctly_wherever_the_stream_breaks() {
        let whole = "<think>why < not</think>\nanswer";
        for at in 0..=whole.len() {
            let (a, b) = whole.split_at(at);
            let got = split_all(&[a, b]);
            assert_eq!(got.thinking, "why < not", "at {at}");
            assert_eq!(got.text, "\nanswer", "at {at}");
        }
    }

    #[test]
    fn a_think_tag_later_in_the_reply_is_ordinary_text() {
        let got = split_all(&["use the ", "<think>tag</think>"]);
        assert_eq!(got.text, "use the <think>tag</think>");
        assert_eq!(got.thinking, "");
    }

    #[test]
    fn an_unfinished_think_block_is_still_thinking() {
        let got = split_all(&["<think>never closed </th"]);
        assert_eq!(got.thinking, "never closed </th");
        assert_eq!(got.text, "");
    }
}
