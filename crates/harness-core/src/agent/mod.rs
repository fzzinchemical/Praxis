//! The agent loop and the events front ends see (SPEC §4).

use crate::permissions::{Decision, Permissions};
use crate::provider::{
    ChatChunk, ChatRequest, FinishReason, Message, Provider, ProviderError, ToolCall,
};
use crate::tools::{self, ToolContext, ToolOutput};
use futures::{StreamExt, pin_mut};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

pub type ToolCallId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Cancelled,
    LoopDetected,
    ProviderError,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolOutcome {
    Completed(ToolOutput),
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionReply {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    Cancelled,
}

#[derive(Debug)]
pub enum AgentEvent {
    TextDelta(String),
    ThinkingDelta(String),
    ToolStarted {
        id: ToolCallId,
        name: String,
        input: Value,
        title: String,
    },
    ToolFinished {
        id: ToolCallId,
        outcome: ToolOutcome,
    },
    /// Show the full `input`. Offer "always" only when `can_always`. Dropping `respond` denies.
    PermissionRequest {
        id: ToolCallId,
        name: String,
        input: Value,
        can_always: bool,
        respond: oneshot::Sender<PermissionReply>,
    },
    Usage {
        used_tokens: u32,
        context_size: u32,
        cached_tokens: Option<u32>,
    },
    Notice {
        severity: Severity,
        title: String,
        detail: Option<String>,
    },
    TurnEnded(StopReason),
}

pub struct AgentConfig {
    /// A model key or, preferably, the instance id from `Provider::ensure_ready`.
    pub model: String,
    pub context_size: u32,
    pub system_prompt: String,
    pub max_turn_requests: u32,
}

pub const DEFAULT_MAX_TURN_REQUESTS: u32 = 50;

const NOT_RUN_RESULT: &str = "Cancelled by the user before this call ran.";
const INTERRUPTED_REPLY: &str = "(interrupted)";

/// One conversation. History stays valid for the next prompt however a turn ends: every tool
/// call has a result and every user message a reply, which strict chat templates insist on.
pub struct Agent {
    provider: Arc<Provider>,
    tools: Arc<ToolContext>,
    config: AgentConfig,
    history: Vec<Message>,
    permissions: Permissions,
}

/// What one model request produced before it ended.
#[derive(Default)]
struct Reply {
    text: String,
    calls: Vec<ToolCall>,
    finish: Option<FinishReason>,
    error: Option<ProviderError>,
}

impl Agent {
    pub fn new(provider: Arc<Provider>, tools: Arc<ToolContext>, config: AgentConfig) -> Self {
        let history = vec![Message::System {
            content: config.system_prompt.clone(),
        }];
        Self {
            provider,
            tools,
            config,
            history,
            permissions: Permissions::default(),
        }
    }

    /// Runs one prompt to completion. `TurnEnded` is always the last event. If the receiver
    /// goes away nobody can steer the turn any more, so it ends as cancelled.
    pub async fn run_turn(
        &mut self,
        prompt: String,
        events: &mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
    ) {
        self.history.push(Message::User { content: prompt });
        let turn = Turn {
            events,
            cancel: cancel.child_token(),
        };
        let reason = self.run_requests(&turn).await;
        turn.emit(AgentEvent::TurnEnded(reason)).await;
    }

    async fn run_requests(&mut self, turn: &Turn<'_>) -> StopReason {
        let mut requests = 0;
        loop {
            requests += 1;
            let reply = self.request(turn).await;
            if let Some(error) = reply.error {
                // Half-streamed tool calls never arrive, so only text can need keeping.
                let content = match reply.text.as_str() {
                    "" => INTERRUPTED_REPLY.to_owned(),
                    _ => reply.text,
                };
                self.history.push(Message::Assistant {
                    content: Some(content),
                    tool_calls: Vec::new(),
                });
                return turn.provider_failure(error).await;
            }
            let done = reply.calls.is_empty();
            self.history.push(Message::Assistant {
                content: (done || !reply.text.is_empty()).then_some(reply.text),
                tool_calls: reply.calls.clone(),
            });
            if done {
                return match reply.finish {
                    Some(FinishReason::Length) => StopReason::MaxTokens,
                    _ => StopReason::EndTurn,
                };
            }
            for call in reply.calls {
                let content = if turn.cancel.is_cancelled() {
                    NOT_RUN_RESULT.to_owned()
                } else {
                    self.run_call(&call, turn).await
                };
                self.history.push(Message::Tool {
                    tool_call_id: call.id,
                    content,
                });
            }
            if turn.cancel.is_cancelled() {
                return StopReason::Cancelled;
            }
            if requests >= self.config.max_turn_requests {
                return StopReason::MaxTurnRequests;
            }
        }
    }

    async fn request(&self, turn: &Turn<'_>) -> Reply {
        let request = ChatRequest {
            model: self.config.model.clone(),
            messages: self.history.clone(),
            tools: tools::schemas(),
            max_tokens: None,
        };
        let mut reply = Reply::default();
        let stream = match self.provider.chat(request, turn.cancel.clone()).await {
            Ok(stream) => stream,
            Err(error) => {
                reply.error = Some(error);
                return reply;
            }
        };
        pin_mut!(stream);
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(ChatChunk::Text(delta)) => {
                    reply.text.push_str(&delta);
                    turn.emit(AgentEvent::TextDelta(delta)).await;
                }
                Ok(ChatChunk::Thinking(delta)) => {
                    turn.emit(AgentEvent::ThinkingDelta(delta)).await;
                }
                Ok(ChatChunk::ToolCall(call)) => reply.calls.push(call),
                Ok(ChatChunk::Finish(reason)) => reply.finish = Some(reason),
                Ok(ChatChunk::Usage(usage)) => {
                    turn.emit(AgentEvent::Usage {
                        used_tokens: usage.prompt_tokens + usage.completion_tokens,
                        context_size: self.config.context_size,
                        // LM Studio reports no cached-token field (SPEC §5.2).
                        cached_tokens: None,
                    })
                    .await;
                }
                Err(error) => {
                    reply.error = Some(error);
                    break;
                }
            }
        }
        reply
    }

    /// Returns the result to show the model.
    async fn run_call(&mut self, call: &ToolCall, turn: &Turn<'_>) -> String {
        let input = parse_arguments(&call.arguments);
        turn.emit(AgentEvent::ToolStarted {
            id: call.id.clone(),
            name: call.name.clone(),
            input: input.clone().unwrap_or(Value::Null),
            title: title(&call.name, input.as_ref()),
        })
        .await;
        let output = match input {
            Some(input) => match self.check_permission(call, &input, turn).await {
                Ok(()) => {
                    let (tools, cancel) = (Arc::clone(&self.tools), turn.cancel.clone());
                    tools::execute(&call.name, input, tools, cancel).await
                }
                Err(reason) => ToolOutput::error(reason),
            },
            None => ToolOutput::error(
                "Invalid arguments: expected a JSON object. Call the tool again with valid JSON."
                    .to_owned(),
            ),
        };
        let content = output.content.clone();
        // A tool that succeeded despite the cancel keeps its result: the work happened.
        let outcome = if turn.cancel.is_cancelled() && output.is_error {
            ToolOutcome::Cancelled
        } else {
            ToolOutcome::Completed(output)
        };
        turn.emit(AgentEvent::ToolFinished {
            id: call.id.clone(),
            outcome,
        })
        .await;
        content
    }

    /// `Err` holds the denial to show the model. Anything but an explicit approval denies.
    async fn check_permission(
        &mut self,
        call: &ToolCall,
        input: &Value,
        turn: &Turn<'_>,
    ) -> Result<(), String> {
        let can_always = match self.permissions.decide(&call.name) {
            Decision::Allow => return Ok(()),
            Decision::Deny(reason) => return Err(reason),
            Decision::Ask { can_always } => can_always,
        };
        let (respond, answer) = oneshot::channel();
        turn.emit(AgentEvent::PermissionRequest {
            id: call.id.clone(),
            name: call.name.clone(),
            input: input.clone(),
            can_always,
            respond,
        })
        .await;
        let reply = tokio::select! {
            answer = answer => answer.unwrap_or(PermissionReply::RejectOnce),
            () = turn.cancel.cancelled() => PermissionReply::Cancelled,
        };
        if reply == PermissionReply::Cancelled {
            turn.cancel.cancel();
        }
        self.permissions.record(&call.name, reply)
    }
}

struct Turn<'a> {
    events: &'a mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
}

impl Turn<'_> {
    async fn emit(&self, event: AgentEvent) {
        if self.events.send(event).await.is_err() {
            self.cancel.cancel();
        }
    }

    async fn provider_failure(&self, error: ProviderError) -> StopReason {
        if matches!(error, ProviderError::Cancelled) || self.cancel.is_cancelled() {
            return StopReason::Cancelled;
        }
        self.emit(AgentEvent::Notice {
            severity: Severity::Error,
            title: error.to_string(),
            detail: None,
        })
        .await;
        StopReason::ProviderError
    }
}

/// Model arguments are untrusted text, so anything but a JSON object goes back as an error.
fn parse_arguments(arguments: &str) -> Option<Value> {
    serde_json::from_str(arguments)
        .ok()
        .filter(Value::is_object)
}

fn title(name: &str, input: Option<&Value>) -> String {
    let subject = ["file_path", "command", "pattern"]
        .iter()
        .find_map(|key| input?.get(key)?.as_str());
    match subject {
        Some(subject) => format!("{name} {subject}"),
        None => name.to_owned(),
    }
}
