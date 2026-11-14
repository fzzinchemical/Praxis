//! Provider tests over HTTP against a fake server with recorded fixtures (SPEC §14).

// Lets clippy treat helper functions here as test code for `allow-unwrap-in-tests`.
#![cfg(test)]

use futures::{StreamExt, pin_mut};
use praxis_core::provider::{
    ChatChunk, ChatRequest, FinishReason, LoadPolicy, Message, Provider, ProviderError, Sampling,
    ToolCall, ToolSchema,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CHAT: &str = "/v1/chat/completions";

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lmstudio")
}

fn fixture(relative: &str) -> Vec<u8> {
    std::fs::read(fixtures_root().join(relative)).unwrap()
}

fn sse(bytes: Vec<u8>) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(bytes, "text/event-stream")
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "qwen3.5-4b".into(),
        messages: vec![Message::User {
            content: "hi".into(),
        }],
        tools: vec![],
        max_tokens: None,
        sampling: Sampling::default(),
        seed: None,
        stop: Vec::new(),
        response_format: None,
    }
}

async fn serve(response: ResponseTemplate) -> (MockServer, Provider) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(CHAT))
        .respond_with(response)
        .mount(&server)
        .await;
    let provider = Provider::new(&server.uri(), None).unwrap();
    (server, provider)
}

async fn collect(provider: &Provider, req: ChatRequest) -> Vec<Result<ChatChunk, ProviderError>> {
    let stream = provider.chat(req, CancellationToken::new()).await.unwrap();
    stream.collect().await
}

async fn chat_error(response: ResponseTemplate) -> ProviderError {
    let (_server, provider) = serve(response).await;
    provider
        .chat(request(), CancellationToken::new())
        .await
        .err()
        .unwrap()
}

fn error_body(code: Value, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(json!({"error": {
        "message": message, "type": "invalid_request", "param": null, "code": code,
    }}))
}

fn text_of(items: &[Result<ChatChunk, ProviderError>]) -> (String, String) {
    let (mut text, mut thinking) = (String::new(), String::new());
    for item in items {
        match item {
            Ok(ChatChunk::Text(t)) => text.push_str(t),
            Ok(ChatChunk::Thinking(t)) => thinking.push_str(t),
            _ => {}
        }
    }
    (text, thinking)
}

#[tokio::test]
async fn streams_a_text_reply_with_thinking_usage_and_finish() {
    let (_s, provider) = serve(sse(fixture("qwen3.5-4b/no_tool_needed_0.sse"))).await;
    let items = collect(&provider, request()).await;

    let (text, thinking) = text_of(&items);
    assert!(!text.is_empty());
    assert!(!thinking.is_empty());
    assert!(items.iter().all(Result::is_ok));
    assert!(matches!(
        items.iter().rev().nth(1),
        Some(Ok(ChatChunk::Finish(FinishReason::Stop)))
    ));
    let Some(Ok(ChatChunk::Usage(usage))) = items.last() else {
        panic!("usage must be last: {:?}", items.last());
    };
    assert!(usage.prompt_tokens > 0);
    assert!(usage.completion_tokens_details.reasoning_tokens > 0);
}

#[tokio::test]
async fn assembles_parallel_tool_calls_by_index() {
    let (_s, provider) = serve(sse(fixture("qwen3.5-4b/tool_parallel_0.sse"))).await;
    let items = collect(&provider, request()).await;

    let calls: Vec<&ToolCall> = items
        .iter()
        .filter_map(|i| match i {
            Ok(ChatChunk::ToolCall(call)) => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 2);
    assert_ne!(calls[0].id, calls[1].id);
    assert_eq!(calls[0].name, "Read");
    let first: Value = serde_json::from_str(&calls[0].arguments).unwrap();
    assert_eq!(first, json!({"file_path": "src/a.rs"}));
    let second: Value = serde_json::from_str(&calls[1].arguments).unwrap();
    assert!(second["file_path"].is_string());
    assert_ne!(first, second);
    assert!(
        items
            .iter()
            .any(|i| matches!(i, Ok(ChatChunk::Finish(FinishReason::ToolCalls))))
    );
}

#[tokio::test]
async fn keeps_fenced_tool_text_from_a_non_native_model_as_text() {
    let (_s, provider) = serve(sse(fixture(
        "qwen2.5-coder-7b-instruct/tool_parallel_0.sse",
    )))
    .await;
    let items = collect(&provider, request()).await;
    assert!(text_of(&items).0.contains("<tools>"));
    assert!(
        !items
            .iter()
            .any(|i| matches!(i, Ok(ChatChunk::ToolCall(_))))
    );
}

#[tokio::test]
async fn http_200_context_overflow_becomes_a_typed_error() {
    let (_s, provider) = serve(sse(fixture("qwen2.5-coder-7b-instruct/oversize_1.5.sse"))).await;
    let items = collect(&provider, request()).await;
    assert_eq!(items.len(), 1);
    assert!(matches!(
        items[0],
        Err(ProviderError::ContextOverflow {
            prompt_tokens: Some(12182),
            context_size: Some(8192),
        })
    ));
}

#[tokio::test]
async fn context_overflow_with_unparseable_details_is_still_typed() {
    let body = b"event: error\ndata: {\"error\":{\"message\":\"exceed_context_size_error\"}}\n\n";
    let (_s, provider) = serve(sse(body.to_vec())).await;
    let items = collect(&provider, request()).await;
    assert!(matches!(
        items[0],
        Err(ProviderError::ContextOverflow {
            prompt_tokens: None,
            context_size: None,
        })
    ));
}

#[tokio::test]
async fn other_sse_errors_carry_the_server_message() {
    let (_s, provider) = serve(sse(fixture("qwen3.5-4b/response_format_with_tools.sse"))).await;
    let items = collect(&provider, request()).await;
    let Err(ProviderError::Server { message }) = &items[0] else {
        panic!("{:?}", items[0]);
    };
    assert!(message.contains("failed to parse grammar"));
}

#[tokio::test]
async fn every_recorded_stream_decodes_without_a_protocol_failure() {
    for model in std::fs::read_dir(fixtures_root()).unwrap() {
        let dir = model.unwrap().path();
        if !dir.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(dir).unwrap() {
            let file = file.unwrap().path();
            if file.extension().is_none_or(|e| e != "sse") {
                continue;
            }
            let (_s, provider) = serve(sse(std::fs::read(&file).unwrap())).await;
            for item in collect(&provider, request()).await {
                if let Err(e) = item {
                    assert!(
                        matches!(
                            e,
                            ProviderError::ContextOverflow { .. } | ProviderError::Server { .. }
                        ),
                        "{file:?}: {e}"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn sends_an_openai_shaped_request_with_auth() {
    let server = MockServer::start().await;
    let expected = json!({
        "model": "qwen3.5-4b",
        "stream": true,
        "stream_options": {"include_usage": true},
        "max_tokens": 16,
        "seed": 42,
        "messages": [
            {"role": "system", "content": "s"},
            {"role": "user", "content": "u"},
            {"role": "assistant", "content": null, "tool_calls": [{
                "id": "1", "type": "function",
                "function": {"name": "Read", "arguments": "{}"},
            }]},
            {"role": "tool", "tool_call_id": "1", "content": "r"},
        ],
        "tools": [{"type": "function", "function": {
            "name": "Read", "description": "d", "parameters": {"type": "object"},
        }}],
    });
    Mock::given(method("POST"))
        .and(path(CHAT))
        .and(header("authorization", "Bearer key"))
        .and(body_partial_json(expected))
        .respond_with(sse(fixture("qwen3.5-4b/no_tool_needed_0.sse")))
        .expect(1)
        .mount(&server)
        .await;
    let provider = Provider::new(&server.uri(), Some("key".into())).unwrap();
    let req = ChatRequest {
        model: "qwen3.5-4b".into(),
        messages: vec![
            Message::System {
                content: "s".into(),
            },
            Message::User {
                content: "u".into(),
            },
            Message::Assistant {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "1".into(),
                    name: "Read".into(),
                    arguments: "{}".into(),
                }],
            },
            Message::Tool {
                tool_call_id: "1".into(),
                content: "r".into(),
            },
        ],
        tools: vec![ToolSchema {
            name: "Read".into(),
            description: "d".into(),
            parameters: json!({"type": "object"}),
        }],
        max_tokens: Some(16),
        sampling: Sampling::default(),
        seed: Some(42),
        stop: Vec::new(),
        response_format: None,
    };
    assert!(collect(&provider, req).await.iter().all(Result::is_ok));
}

#[tokio::test]
async fn server_down_is_reported_as_unreachable() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let provider = Provider::new(&format!("http://127.0.0.1:{port}"), None).unwrap();

    let chat = provider.chat(request(), CancellationToken::new()).await;
    assert!(matches!(
        chat.err(),
        Some(ProviderError::Unreachable { .. })
    ));
    assert!(matches!(
        provider.list_models().await,
        Err(ProviderError::Unreachable { .. })
    ));
}

#[tokio::test]
async fn no_model_loaded_is_a_distinct_error() {
    let error = chat_error(error_body(
        Value::Null,
        "No models loaded. Please load one.",
    ))
    .await;
    assert!(matches!(error, ProviderError::NoModelLoaded));
}

#[tokio::test]
async fn unknown_model_is_a_distinct_error() {
    let error = chat_error(error_body(json!("model_not_found"), "Model 'x' not found")).await;
    assert!(matches!(error, ProviderError::ModelNotFound { .. }));
}

#[tokio::test]
async fn unsupported_tools_is_a_distinct_error() {
    let error = chat_error(error_body(Value::Null, "This model does not support tools")).await;
    assert!(matches!(error, ProviderError::ToolsUnsupported { .. }));
}

#[tokio::test]
async fn a_non_json_error_response_keeps_its_body() {
    let error = chat_error(ResponseTemplate::new(500).set_body_string("boom")).await;
    let ProviderError::Server { message } = error else {
        panic!("{error:?}");
    };
    assert!(message.contains("boom"));
}

#[tokio::test]
async fn cancel_after_the_first_chunk_ends_the_stream() {
    let (_s, provider) = serve(sse(fixture("qwen3.5-4b/no_tool_needed_0.sse"))).await;
    let cancel = CancellationToken::new();
    let stream = provider.chat(request(), cancel.clone()).await.unwrap();
    pin_mut!(stream);

    assert!(matches!(stream.next().await, Some(Ok(_))));
    cancel.cancel();
    assert!(matches!(
        stream.next().await,
        Some(Err(ProviderError::Cancelled))
    ));
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn cancel_interrupts_a_request_with_no_response_yet() {
    let (_s, provider) = serve(sse(vec![]).set_delay(Duration::from_secs(60))).await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = provider.chat(request(), cancel).await;
    assert!(matches!(result.err(), Some(ProviderError::Cancelled)));
}

/// wiremock sends a body all at once, so a body that stalls needs a raw socket.
#[tokio::test]
async fn cancel_interrupts_a_stalled_body() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0; 4096];
        let _ = socket.read(&mut buf).await.unwrap();
        let body =
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":null}]}\n\n";
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n",
            body.len()
        );
        socket.write_all(head.as_bytes()).await.unwrap();
        // Stay open and silent until the client hangs up, proving the drop aborts the request.
        let _ = socket.read(&mut buf).await;
    });
    let provider = Provider::new(&format!("http://127.0.0.1:{port}"), None).unwrap();
    let cancel = CancellationToken::new();
    let stream = provider.chat(request(), cancel.clone()).await.unwrap();
    pin_mut!(stream);

    assert!(matches!(stream.next().await, Some(Ok(ChatChunk::Text(_)))));
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
    });
    let item = tokio::time::timeout(Duration::from_secs(5), stream.next()).await;
    assert!(matches!(item, Ok(Some(Err(ProviderError::Cancelled)))));
    canceller.await.unwrap();
    server.abort();
}

#[tokio::test]
async fn a_stream_ending_before_finish_reports_truncation() {
    let body = b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":null}]}\n\n";
    let (_s, provider) = serve(sse(body.to_vec())).await;
    let items = collect(&provider, request()).await;
    assert!(matches!(items[0], Ok(ChatChunk::Text(_))));
    assert!(matches!(items[1], Err(ProviderError::Truncated)));
    assert_eq!(items.len(), 2);
}

#[tokio::test]
async fn an_unparseable_chunk_is_a_malformed_error() {
    let (_s, provider) = serve(sse(b"data: {nope\n\n".to_vec())).await;
    let items = collect(&provider, request()).await;
    assert!(matches!(items[0], Err(ProviderError::Malformed { .. })));
    assert_eq!(items.len(), 1);
}

#[tokio::test]
async fn a_tool_call_without_a_name_is_a_malformed_error() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"1\",",
        "\"function\":{\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
    );
    let (_s, provider) = serve(sse(body.as_bytes().to_vec())).await;
    let items = collect(&provider, request()).await;
    assert!(matches!(items[0], Err(ProviderError::Malformed { .. })));
}

#[tokio::test]
async fn lists_models_from_the_native_api() {
    let server = MockServer::start().await;
    // The recording kept only the selected model, unwrapped; the route returns `{"models": [...]}`.
    let models: Value =
        serde_json::from_slice(&fixture("qwen3.5-4b/models.response.json")).unwrap();
    Mock::given(method("GET"))
        .and(path("/api/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": models})))
        .mount(&server)
        .await;
    let provider = Provider::new(&server.uri(), None).unwrap();

    let models = provider.list_models().await.unwrap();
    assert_eq!(models[0].key, "qwen3.5-4b");
    assert_eq!(models[0].max_context_length, 262144);
    assert!(models[0].capabilities.trained_for_tool_use);
    assert_eq!(models[0].loaded_instances[0].id, "qwen3.5-4b");
    assert_eq!(models[0].loaded_instances[0].config.context_length, 8192);
}

#[test]
fn rejects_a_base_url_that_is_not_http() {
    for url in ["not a url", "file:///etc/passwd"] {
        assert!(matches!(
            Provider::new(url, None),
            Err(ProviderError::InvalidUrl { .. })
        ));
    }
}

const QWEN: &str = "qwen3.5-4b";

fn recorded_models() -> Value {
    serde_json::from_slice(&fixture("qwen3.5-4b/models.response.json")).unwrap()
}

fn recorded_load() -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_body_raw(fixture("qwen3.5-4b/load.response.json"), "application/json")
}

/// Serves `models` wrapped as the real route does. The load route must be called exactly `loads` times.
async fn admin_server(models: Value, load: ResponseTemplate, loads: u64) -> (MockServer, Provider) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "models": models })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/models/load"))
        .and(body_partial_json(
            json!({"model": QWEN, "echo_load_config": true}),
        ))
        .respond_with(load)
        .expect(loads)
        .mount(&server)
        .await;
    let provider = Provider::new(&server.uri(), None).unwrap();
    (server, provider)
}

async fn ready_error(provider: &Provider, model: &str, policy: LoadPolicy) -> ProviderError {
    provider
        .ensure_ready(model, 16000, policy)
        .await
        .err()
        .unwrap()
}

#[tokio::test]
async fn enough_context_uses_the_loaded_instance_without_reloading() {
    let (_s, provider) = admin_server(recorded_models(), recorded_load(), 0).await;
    for context in [4096, 8192] {
        let ready = provider
            .ensure_ready(QWEN, context, LoadPolicy::Allowed)
            .await
            .unwrap();
        assert_eq!(ready.instance_id, QWEN);
        assert_eq!(ready.context_length, 8192);
    }
}

#[tokio::test]
async fn picks_the_instance_with_enough_context_when_several_are_loaded() {
    let mut models = recorded_models();
    let mut big = models[0]["loaded_instances"][0].clone();
    big["id"] = json!("qwen3.5-4b:2");
    big["config"]["context_length"] = json!(32000);
    models[0]["loaded_instances"]
        .as_array_mut()
        .unwrap()
        .push(big);
    let (_s, provider) = admin_server(models, recorded_load(), 0).await;
    let ready = provider
        .ensure_ready(QWEN, 16000, LoadPolicy::AskFirst)
        .await
        .unwrap();
    assert_eq!(ready.instance_id, "qwen3.5-4b:2");
    assert_eq!(ready.context_length, 32000);
}

#[tokio::test]
async fn too_little_context_without_consent_asks_and_does_not_load() {
    let (_s, provider) = admin_server(recorded_models(), recorded_load(), 0).await;
    let error = ready_error(&provider, QWEN, LoadPolicy::AskFirst).await;
    let ProviderError::LoadNeedsConsent { model, context } = &error else {
        panic!("{error:?}");
    };
    assert_eq!((model.as_str(), *context), (QWEN, 16000));
    assert!(error.to_string().contains("auto_load"));
}

#[tokio::test]
async fn a_model_not_loaded_at_all_also_needs_consent() {
    let mut models = recorded_models();
    models[0]["loaded_instances"] = json!([]);
    let (_s, provider) = admin_server(models, recorded_load(), 0).await;
    let error = ready_error(&provider, QWEN, LoadPolicy::AskFirst).await;
    assert!(matches!(error, ProviderError::LoadNeedsConsent { .. }));
}

#[tokio::test]
async fn too_little_context_with_consent_loads_with_the_requested_context() {
    let mut models = recorded_models();
    models[0]["loaded_instances"][0]["config"]["context_length"] = json!(4096);
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "models": models })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/models/load"))
        .and(body_partial_json(
            json!({"model": QWEN, "context_length": 8192}),
        ))
        .respond_with(recorded_load())
        .expect(1)
        .mount(&server)
        .await;
    let provider = Provider::new(&server.uri(), None).unwrap();
    let ready = provider
        .ensure_ready(QWEN, 8192, LoadPolicy::Allowed)
        .await
        .unwrap();
    assert_eq!(ready.instance_id, QWEN);
    assert_eq!(ready.context_length, 8192);
}

#[tokio::test]
async fn a_load_that_applied_less_context_than_requested_is_an_error() {
    let (_s, provider) = admin_server(recorded_models(), recorded_load(), 1).await;
    let error = ready_error(&provider, QWEN, LoadPolicy::Allowed).await;
    let ProviderError::Server { message } = error else {
        panic!("{error:?}");
    };
    assert!(
        message.contains("8192") && message.contains("16000"),
        "{message}"
    );
}

#[tokio::test]
async fn an_unreadable_load_response_is_malformed() {
    let load = ResponseTemplate::new(200).set_body_json(json!({"status": "loaded"}));
    let (_s, provider) = admin_server(recorded_models(), load, 1).await;
    let error = ready_error(&provider, QWEN, LoadPolicy::Allowed).await;
    assert!(matches!(error, ProviderError::Malformed { .. }));
}

#[tokio::test]
async fn a_failed_load_reports_the_server_error() {
    let load = error_body(Value::Null, "Not enough memory");
    let (_s, provider) = admin_server(recorded_models(), load, 1).await;
    let error = ready_error(&provider, QWEN, LoadPolicy::Allowed).await;
    let ProviderError::Server { message } = error else {
        panic!("{error:?}");
    };
    assert!(message.contains("Not enough memory"));
}

#[tokio::test]
async fn a_missing_model_is_an_error_and_never_loads_even_with_consent() {
    let (_s, provider) = admin_server(recorded_models(), recorded_load(), 0).await;
    let error = ready_error(&provider, "no/such-model", LoadPolicy::Allowed).await;
    let ProviderError::ModelNotFound { message } = error else {
        panic!("{error:?}");
    };
    assert!(message.contains("no/such-model"));
}
