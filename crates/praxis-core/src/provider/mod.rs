//! LM Studio client: chat over the OpenAI-compatible route, model management
//! over the native API (SPEC §5).

mod admin;
mod chat;
pub mod stream;

pub use admin::{Capabilities, InstanceConfig, LoadPolicy, LoadedInstance, ModelInfo, ReadyModel};
pub use chat::{
    ChatChunk, ChatRequest, CompletionDetails, FinishReason, MEMORY_MARK, Message, Sampling,
    ToolCall, ToolSchema, Usage,
};

use admin::AdminClient;
use chat::ChatClient;
use futures::Stream;
use reqwest::{Client, RequestBuilder, Response, Url};
use std::net::IpAddr;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

/// Each message says what happened and what the user can do about it (SPEC §15).
#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("`{url}` is not a usable http(s) server URL. Fix the base URL in your config.")]
    InvalidUrl { url: String },
    #[error("Can't reach LM Studio at {url}. Start its local server and check the base URL.")]
    Unreachable { url: String },
    #[error("LM Studio has no model loaded. Load a model and try again.")]
    NoModelLoaded,
    #[error("LM Studio doesn't know this model: {message}. Check the model name.")]
    ModelNotFound { message: String },
    #[error(
        "`{model}` needs loading with {context} context, which disrupts anything else using LM Studio. Approve the load, list it in `[models]` with `auto_load = true`, or load it yourself."
    )]
    LoadNeedsConsent { model: String, context: u32 },
    #[error(
        "The prompt doesn't fit the model's context. Compact the conversation or load the model with more context."
    )]
    ContextOverflow {
        prompt_tokens: Option<u32>,
        context_size: Option<u32>,
    },
    #[error("This model can't use tools: {message}. Choose a model with tool support.")]
    ToolsUnsupported { message: String },
    #[error("LM Studio reported an error: {message}")]
    Server { message: String },
    /// A 5xx answer, kept apart from `Server` so it can be retried (T13.2).
    #[error("LM Studio returned HTTP {status}: {message}")]
    HttpStatus { status: u16, message: String },
    #[error("LM Studio sent a response this client can't read: {detail}")]
    Malformed { detail: String },
    #[error("The response stopped before the model finished. Check LM Studio and try again.")]
    Truncated,
    #[error("The connection to LM Studio failed. Check that its server is still running.")]
    Transport(#[source] reqwest::Error),
    #[error("Cancelled.")]
    Cancelled,
}

/// Maps a server error, from an HTTP status or an SSE `event: error`, to its type.
fn classify_error(code: Option<&str>, message: String) -> ProviderError {
    if message.contains("exceed_context_size_error") {
        let (prompt_tokens, context_size) = overflow_sizes(&message);
        return ProviderError::ContextOverflow {
            prompt_tokens,
            context_size,
        };
    }
    // LM Studio's HTTP 500 form of overflow carries no token counts, so the sizes stay unknown.
    if message
        .to_lowercase()
        .contains("context size has been exceeded")
    {
        return ProviderError::ContextOverflow {
            prompt_tokens: None,
            context_size: None,
        };
    }
    if code == Some("model_not_found") {
        return ProviderError::ModelNotFound { message };
    }
    if message.contains("No models loaded") {
        return ProviderError::NoModelLoaded;
    }
    // shortcut: no recording of this error exists, so the wording is a guess; replace it once a real one is recorded.
    let lower = message.to_lowercase();
    if lower.contains("support") && lower.contains("tool") {
        return ProviderError::ToolsUnsupported { message };
    }
    ProviderError::Server { message }
}

/// The sizes sit in JSON embedded in the message text (ADR 0002).
fn overflow_sizes(message: &str) -> (Option<u32>, Option<u32>) {
    let inner = message
        .find('{')
        .and_then(|start| serde_json::from_str::<serde_json::Value>(&message[start..]).ok());
    let size = |field: &str| {
        let value = inner.as_ref()?.get("error")?.get(field)?.as_u64()?;
        u32::try_from(value).ok()
    };
    (size("n_prompt_tokens"), size("n_ctx"))
}

/// Reads `{"error": {"code", "message"}}`, the shape of both HTTP and SSE errors.
fn parse_error_body(body: &str) -> ProviderError {
    let value: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let error = &value["error"];
    let message = error["message"]
        .as_str()
        .or(error.as_str())
        .unwrap_or(body)
        .to_owned();
    classify_error(error["code"].as_str(), message)
}

pub(crate) fn is_loopback(url: &Url) -> bool {
    let host = url.host_str().unwrap_or_default();
    host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Connection details shared by the chat and admin clients. No `Debug`: it holds the key.
#[derive(Clone)]
struct Endpoint {
    http: Client,
    base_url: String,
    api_key: Option<String>,
}

impl Endpoint {
    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.base_url)
    }

    fn with_auth(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.api_key {
            Some(key) => request.bearer_auth(key),
            None => request,
        }
    }

    fn send_error(&self, error: reqwest::Error) -> ProviderError {
        if error.is_connect() {
            ProviderError::Unreachable {
                url: self.base_url.clone(),
            }
        } else {
            ProviderError::Transport(error)
        }
    }

    /// Sends the request, turning a failed status into a typed error.
    async fn send(&self, request: RequestBuilder) -> Result<Response, ProviderError> {
        let response = self
            .with_auth(request)
            .send()
            .await
            .map_err(|e| self.send_error(e))?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let body = response.text().await.map_err(ProviderError::Transport)?;
        Err(match parse_error_body(&body) {
            ProviderError::Server { message } if status.is_server_error() => {
                ProviderError::HttpStatus {
                    status: status.as_u16(),
                    message,
                }
            }
            ProviderError::Server { message } => ProviderError::Server {
                message: format!("HTTP {status}: {message}"),
            },
            typed => typed,
        })
    }
}

pub struct Provider {
    chat: ChatClient,
    admin: AdminClient,
}

impl Provider {
    /// `api_key` is sent as a bearer token when present. Loopback URLs bypass any
    /// configured proxy; a non-loopback plain `http://` URL logs a warning.
    pub fn new(base_url: &str, api_key: Option<String>) -> Result<Self, ProviderError> {
        let invalid = || ProviderError::InvalidUrl {
            url: base_url.to_owned(),
        };
        let url = Url::parse(base_url).map_err(|_| invalid())?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(invalid());
        }
        let loopback = is_loopback(&url);
        if url.scheme() == "http" && !loopback {
            tracing::warn!(%base_url, "plain http to a remote server exposes prompts and any API key");
        }
        let mut builder = Client::builder();
        if loopback {
            builder = builder.no_proxy();
        }
        let endpoint = Endpoint {
            http: builder.build().map_err(ProviderError::Transport)?,
            base_url: base_url.trim_end_matches('/').to_owned(),
            api_key,
        };
        Ok(Self {
            chat: ChatClient::new(endpoint.clone()),
            admin: AdminClient::new(endpoint),
        })
    }

    /// Starts a chat. Errors arrive here for a failed status, and in the stream
    /// after that, including context overflow, which LM Studio sends on HTTP 200.
    /// Cancelling yields one `ProviderError::Cancelled`, then the stream ends.
    /// Dropping the stream aborts the request.
    pub async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<impl Stream<Item = Result<ChatChunk, ProviderError>>, ProviderError> {
        self.chat.chat(req, cancel).await
    }

    pub async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.admin.list_models().await
    }

    /// Finds a loaded instance of `model` with at least `context`, loading one if
    /// `policy` allows. A new load leaves existing instances in place.
    pub async fn ensure_ready(
        &self,
        model: &str,
        context: u32,
        policy: LoadPolicy,
    ) -> Result<ReadyModel, ProviderError> {
        self.admin.ensure_ready(model, context, policy).await
    }

    pub async fn unload(&self, instance_id: &str) -> Result<(), ProviderError> {
        self.admin.unload(instance_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_loopback_hosts_only() {
        for (url, expected) in [
            ("http://localhost:1234", true),
            ("http://127.0.0.1:1234", true),
            ("http://127.1.2.3", true),
            ("http://[::1]:1234", true),
            ("http://192.168.1.2:1234", false),
            ("http://localhost.example.com", false),
        ] {
            assert_eq!(is_loopback(&Url::parse(url).unwrap()), expected, "{url}");
        }
    }
}
