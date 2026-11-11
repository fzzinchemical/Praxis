//! Streaming chat over `/v1/chat/completions` (SPEC §5).

use super::stream::{SseDecoder, SseEvent};
use super::{Endpoint, ProviderError, parse_error_body};
use futures::{Stream, stream};
use reqwest::Response;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// A model key or instance id.
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f64>,
    /// Makes sampling repeatable where the server supports it (evals, SPEC §14).
    pub seed: Option<u64>,
    pub stop: Vec<String>,
    /// Constrains the reply's `content` to this tool's arguments. LM Studio then returns no
    /// `tool_calls`, and `tools` must stay empty or some models fail (SPEC §5.3).
    pub response_format: Option<ToolSchema>,
}

#[derive(Debug, Clone)]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Debug, Clone)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// A fully assembled call. `arguments` stays the raw model text: it may be invalid
/// JSON, and judging that belongs to tool-call recovery (SPEC §5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    Other(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    #[serde(default)]
    pub completion_tokens_details: CompletionDetails,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct CompletionDetails {
    #[serde(default)]
    pub reasoning_tokens: u32,
}

/// Tool calls arrive whole, in index order, just before `Finish`.
/// `Usage` follows `Finish` when the server sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatChunk {
    Text(String),
    Thinking(String),
    ToolCall(ToolCall),
    Finish(FinishReason),
    Usage(Usage),
}

impl ChatRequest {
    /// Size of the request as sent, which is what token estimates are calibrated against.
    pub fn chars(&self) -> usize {
        request_body(self).to_string().chars().count()
    }
}

impl Message {
    /// Size of this message as sent.
    pub fn chars(&self) -> usize {
        message_json(self).to_string().chars().count()
    }
}

fn request_body(req: &ChatRequest) -> Value {
    let messages: Vec<Value> = req.messages.iter().map(message_json).collect();
    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({"type": "function", "function": {
                    "name": t.name, "description": t.description, "parameters": t.parameters,
                }})
            })
            .collect();
        body["tools"] = tools.into();
    }
    if let Some(max_tokens) = req.max_tokens {
        body["max_tokens"] = max_tokens.into();
    }
    if let Some(tool) = &req.response_format {
        body["response_format"] = json!({"type": "json_schema", "json_schema": {
            "name": tool.name, "strict": true, "schema": tool.parameters,
        }});
    }
    if let Some(temperature) = req.temperature {
        body["temperature"] = temperature.into();
    }
    if let Some(seed) = req.seed {
        body["seed"] = seed.into();
    }
    if !req.stop.is_empty() {
        body["stop"] = req.stop.clone().into();
    }
    body
}

fn message_json(message: &Message) -> Value {
    match message {
        Message::System { content } => json!({"role": "system", "content": content}),
        Message::User { content } => json!({"role": "user", "content": content}),
        Message::Tool {
            tool_call_id,
            content,
        } => json!({"role": "tool", "tool_call_id": tool_call_id, "content": content}),
        Message::Assistant {
            content,
            tool_calls,
        } => {
            let mut value = json!({"role": "assistant", "content": content});
            if !tool_calls.is_empty() {
                let calls: Vec<Value> = tool_calls
                    .iter()
                    .map(|c| {
                        json!({"id": c.id, "type": "function", "function": {
                            "name": c.name, "arguments": c.arguments,
                        }})
                    })
                    .collect();
                value["tool_calls"] = calls.into();
            }
            value
        }
    }
}

#[derive(Deserialize)]
struct WireChunk {
    choices: Vec<WireChoice>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct WireChoice {
    #[serde(default)]
    delta: WireDelta,
    finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct WireDelta {
    content: Option<String>,
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<WireToolDelta>,
}

#[derive(Deserialize)]
struct WireToolDelta {
    index: u32,
    id: Option<String>,
    #[serde(default)]
    function: WireFunctionDelta,
}

#[derive(Default, Deserialize)]
struct WireFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Default)]
struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// Turns SSE events into chunks, assembling tool calls by index.
#[derive(Default)]
struct ChunkAssembler {
    tool_calls: BTreeMap<u32, PartialToolCall>,
    finished: bool,
    done: bool,
}

impl ChunkAssembler {
    fn on_event(&mut self, event: SseEvent) -> Result<Vec<ChatChunk>, ProviderError> {
        // LM Studio reports failures such as context overflow this way on HTTP 200.
        if event.event.as_deref() == Some("error") {
            return Err(parse_error_body(&event.data));
        }
        if event.data == "[DONE]" {
            self.done = true;
            return Ok(Vec::new());
        }
        let chunk: WireChunk =
            serde_json::from_str(&event.data).map_err(|e| ProviderError::Malformed {
                detail: e.to_string(),
            })?;

        let mut out = Vec::new();
        for choice in chunk.choices {
            if let Some(thinking) = choice.delta.reasoning_content.filter(|t| !t.is_empty()) {
                out.push(ChatChunk::Thinking(thinking));
            }
            if let Some(text) = choice.delta.content.filter(|t| !t.is_empty()) {
                out.push(ChatChunk::Text(text));
            }
            for delta in choice.delta.tool_calls {
                let partial = self.tool_calls.entry(delta.index).or_default();
                partial.id = delta.id.or(partial.id.take());
                partial.name = delta.function.name.or(partial.name.take());
                partial
                    .arguments
                    .push_str(&delta.function.arguments.unwrap_or_default());
            }
            if let Some(reason) = choice.finish_reason {
                self.finished = true;
                for partial in std::mem::take(&mut self.tool_calls).into_values() {
                    out.push(ChatChunk::ToolCall(complete(partial)?));
                }
                out.push(ChatChunk::Finish(match reason.as_str() {
                    "stop" => FinishReason::Stop,
                    "length" => FinishReason::Length,
                    "tool_calls" => FinishReason::ToolCalls,
                    _ => FinishReason::Other(reason),
                }));
            }
        }
        out.extend(chunk.usage.map(ChatChunk::Usage));
        Ok(out)
    }
}

fn complete(partial: PartialToolCall) -> Result<ToolCall, ProviderError> {
    let missing = |field: &str| ProviderError::Malformed {
        detail: format!("tool call without {field}"),
    };
    Ok(ToolCall {
        id: partial.id.ok_or_else(|| missing("an id"))?,
        name: partial.name.ok_or_else(|| missing("a name"))?,
        arguments: partial.arguments,
    })
}

struct ChatStream {
    response: Response,
    cancel: CancellationToken,
    decoder: SseDecoder,
    assembler: ChunkAssembler,
    pending: VecDeque<Result<ChatChunk, ProviderError>>,
    ended: bool,
}

impl ChatStream {
    async fn next(&mut self) -> Option<Result<ChatChunk, ProviderError>> {
        loop {
            if self.ended {
                return None;
            }
            if self.cancel.is_cancelled() {
                self.ended = true;
                return Some(Err(ProviderError::Cancelled));
            }
            if let Some(item) = self.pending.pop_front() {
                self.ended = item.is_err();
                return Some(item);
            }
            if self.assembler.done {
                self.ended = true;
                return None;
            }
            let read = tokio::select! {
                biased;
                () = self.cancel.cancelled() => continue,
                read = self.response.chunk() => read,
            };
            match read {
                Ok(Some(bytes)) => self.on_bytes(&bytes),
                Ok(None) if self.assembler.finished => self.assembler.done = true,
                Ok(None) => self.pending.push_back(Err(ProviderError::Truncated)),
                Err(error) => self.pending.push_back(Err(ProviderError::Transport(error))),
            }
        }
    }

    fn on_bytes(&mut self, bytes: &[u8]) {
        for event in self.decoder.push(bytes) {
            match self.assembler.on_event(event) {
                Ok(chunks) => self.pending.extend(chunks.into_iter().map(Ok)),
                Err(error) => {
                    self.pending.push_back(Err(error));
                    return;
                }
            }
            if self.assembler.done {
                return;
            }
        }
    }
}

pub(super) struct ChatClient {
    endpoint: Endpoint,
}

impl ChatClient {
    pub(super) fn new(endpoint: Endpoint) -> Self {
        Self { endpoint }
    }

    pub(super) async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<impl Stream<Item = Result<ChatChunk, ProviderError>>, ProviderError> {
        let url = self.endpoint.url("v1/chat/completions");
        let request = self.endpoint.http.post(url).json(&request_body(&req));
        let response = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(ProviderError::Cancelled),
            response = self.endpoint.send(request) => response?,
        };
        let state = ChatStream {
            response,
            cancel,
            decoder: SseDecoder::default(),
            assembler: ChunkAssembler::default(),
            pending: VecDeque::new(),
            ended: false,
        };
        Ok(stream::unfold(state, |mut state| async move {
            state.next().await.map(|item| (item, state))
        }))
    }
}
