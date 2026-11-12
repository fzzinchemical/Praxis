//! `harness acp`: the agent behind an editor, over ACP v1 on stdio (SPEC §11, ADR 0003).

use agent_client_protocol::v1::{ConnectionTo, schema as v1};
use agent_client_protocol::{
    Agent as AcpAgent, Client, ConnectTo, Error, on_receive_notification, on_receive_request,
};
use harness_core::agent::{Agent, AgentEvent, PermissionReply, StopReason, ToolOutcome};
use harness_core::provider::Message;
use harness_core::session::SessionLog;
use harness_core::tools::EditorReader;
use harness_core::tools::FileDiff;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub type NewAgent = Box<dyn Fn() -> anyhow::Result<Agent> + Send + Sync>;

/// A session's agent is taken out while a turn runs, so a second prompt meanwhile is refused.
struct Slot {
    agent: Option<Agent>,
    cancel: CancellationToken,
}

struct Server {
    new_agent: NewAgent,
    sessions_dir: PathBuf,
    sessions: Mutex<HashMap<String, Slot>>,
    /// Whether the client advertised `fs.readTextFile` at `initialize`.
    client_reads: AtomicBool,
}

impl Server {
    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Slot>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Creates or resumes a session log and returns its id with the messages to replay.
    fn open(
        &self,
        resume: Option<&str>,
        cx: &ConnectionTo<Client>,
    ) -> anyhow::Result<(String, Vec<Message>)> {
        let (log, prior) = match resume {
            Some(id) => SessionLog::resume(&self.sessions_dir, id)?,
            None => (SessionLog::create(&self.sessions_dir)?, Vec::new()),
        };
        let id = log.id().to_owned();
        let agent = (self.new_agent)()?;
        agent
            .tools()
            .checkpoint_into(self.sessions_dir.join(&id).join("checkpoints"));
        if self.client_reads.load(Ordering::Relaxed) {
            agent
                .tools()
                .read_from_editor(editor_reader(cx.clone(), id.clone()));
        }
        let agent = Some(agent.with_session(log, prior.clone()));
        let cancel = CancellationToken::new();
        self.sessions().insert(id.clone(), Slot { agent, cancel });
        Ok((id, prior))
    }

    fn begin_turn(&self, id: &str) -> Result<(Agent, CancellationToken), Error> {
        let mut sessions = self.sessions();
        let slot = sessions
            .get_mut(id)
            .ok_or_else(|| Error::invalid_params().data(format!("unknown session `{id}`")))?;
        let agent = slot
            .agent
            .take()
            .ok_or_else(|| Error::invalid_params().data("a prompt is already running"))?;
        slot.cancel = CancellationToken::new();
        Ok((agent, slot.cancel.clone()))
    }

    fn end_turn(&self, id: &str, agent: Agent) {
        if let Some(slot) = self.sessions().get_mut(id) {
            slot.agent = Some(agent);
        }
    }

    fn cancel(&self, id: &str) {
        if let Some(slot) = self.sessions().get(id) {
            slot.cancel.cancel();
        }
    }
}

pub fn agent(new_agent: NewAgent, sessions_dir: PathBuf) -> impl ConnectTo<Client> {
    let server = Arc::new(Server {
        new_agent,
        sessions_dir,
        sessions: Mutex::default(),
        client_reads: AtomicBool::new(false),
    });
    let (for_init, for_new, for_load, for_prompt, for_cancel) = (
        server.clone(),
        server.clone(),
        server.clone(),
        server.clone(),
        server,
    );
    AcpAgent
        .v1()
        .on_receive_request(
            async move |req: v1::InitializeRequest, responder, _cx| {
                let reads = req.client_capabilities.fs.read_text_file;
                for_init.client_reads.store(reads, Ordering::Relaxed);
                let capabilities = v1::AgentCapabilities::new().load_session(true);
                responder.respond(
                    v1::InitializeResponse::new(req.protocol_version)
                        .agent_capabilities(capabilities),
                )
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async |_req: v1::AuthenticateRequest, responder, _cx| {
                responder.respond(v1::AuthenticateResponse::new())
            },
            on_receive_request!(),
        )
        // shortcut: sessions run in the launch directory and ignore `cwd` and `mcpServers`;
        // T5.2 decides whether the extension can ever pass a different folder.
        .on_receive_request(
            async move |_req: v1::NewSessionRequest, responder, cx| match for_new.open(None, &cx) {
                Ok((id, _)) => responder.respond(v1::NewSessionResponse::new(id)),
                Err(e) => responder.respond_with_internal_error(format!("{e:#}")),
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: v1::LoadSessionRequest, responder, cx| {
                let prior = match for_load.open(Some(&req.session_id.to_string()), &cx) {
                    Ok((_, prior)) => prior,
                    Err(e) => {
                        return responder
                            .respond_with_error(Error::invalid_params().data(e.to_string()));
                    }
                };
                // The client rebuilds the conversation from these before the load answer.
                for update in prior.into_iter().filter_map(replay) {
                    cx.send_notification(v1::SessionNotification::new(
                        req.session_id.clone(),
                        update,
                    ))?;
                }
                responder.respond(v1::LoadSessionResponse::new())
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: v1::PromptRequest, responder, cx| {
                let id = req.session_id.to_string();
                let (mut agent, cancel) = match for_prompt.begin_turn(&id) {
                    Ok(turn) => turn,
                    Err(e) => return responder.respond_with_error(e),
                };
                // shortcut: only text blocks reach the model; images and resources are dropped.
                let prompt: String = req
                    .prompt
                    .iter()
                    .filter_map(|block| match block {
                        v1::ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let server = for_prompt.clone();
                // Awaiting the client inside a handler deadlocks the dispatch loop, so spawn.
                cx.clone().spawn(async move {
                    let (tx, mut rx) = mpsc::channel(64);
                    let turn_cancel = cancel.clone();
                    let task = tokio::spawn(async move {
                        agent.run_turn(prompt, &tx, turn_cancel).await;
                        agent
                    });
                    let mut stop = StopReason::ProviderError;
                    while let Some(event) = rx.recv().await {
                        match event {
                            AgentEvent::PermissionRequest {
                                id: call,
                                name,
                                input,
                                can_always,
                                diff,
                                respond,
                            } => {
                                let update = permission_update(call, name, input, diff);
                                let reply =
                                    ask(&cx, &req.session_id, update, can_always, &cancel).await;
                                // A send failure means the turn already ended, so nothing runs.
                                let _ = respond.send(reply);
                            }
                            AgentEvent::TurnEnded(reason) => stop = reason,
                            other => {
                                if let Some(update) = update(other) {
                                    cx.send_notification(v1::SessionNotification::new(
                                        req.session_id.clone(),
                                        update,
                                    ))?;
                                }
                            }
                        }
                    }
                    let agent = task
                        .await
                        .map_err(|e| Error::internal_error().data(e.to_string()))?;
                    server.end_turn(&id, agent);
                    match stop_reason(stop) {
                        Some(reason) => responder.respond(v1::PromptResponse::new(reason)),
                        None => {
                            responder.respond_with_internal_error(format!("turn failed: {stop:?}"))
                        }
                    }
                })
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            async move |note: v1::CancelNotification, _cx| {
                for_cancel.cancel(&note.session_id.to_string());
                Ok(())
            },
            on_receive_notification!(),
        )
}

/// Reads through the client, so `Read` sees unsaved buffers. Runs on the tool's blocking thread,
/// never in the dispatch loop, so waiting for the client's answer here cannot deadlock.
/// shortcut: a failed request falls back to the disk, which `Read` has already confined.
fn editor_reader(cx: ConnectionTo<Client>, session: String) -> EditorReader {
    EditorReader(Box::new(move |path| {
        let request = v1::ReadTextFileRequest::new(session.clone(), path);
        let answer =
            tokio::runtime::Handle::current().block_on(cx.send_request(request).block_task());
        answer.ok().map(|reply| reply.content)
    }))
}

/// The tool call a permission request shows. A `Write` or `Edit` also carries its diff, so the
/// client can review the change before answering (SPEC §11).
fn permission_update(
    call: String,
    name: String,
    input: serde_json::Value,
    diff: Option<FileDiff>,
) -> v1::ToolCallUpdate {
    let mut fields = v1::ToolCallUpdateFields::new().title(name).raw_input(input);
    if let Some(diff) = diff {
        let text = v1::Diff::new(diff.path, diff.new);
        let text = match diff.old {
            Some(old) => text.old_text(old),
            None => text,
        };
        fields = fields.content(vec![text.into()]);
    }
    v1::ToolCallUpdate::new(call, fields)
}

async fn ask(
    cx: &ConnectionTo<Client>,
    session: &v1::SessionId,
    update: v1::ToolCallUpdate,
    can_always: bool,
    cancel: &CancellationToken,
) -> PermissionReply {
    use v1::PermissionOptionKind as Kind;
    let mut options = vec![(Kind::AllowOnce, "allow_once", "Allow once")];
    if can_always {
        options.push((Kind::AllowAlways, "allow_always", "Allow for this session"));
    }
    options.push((Kind::RejectOnce, "reject_once", "Reject"));
    options.push((
        Kind::RejectAlways,
        "reject_always",
        "Reject for this session",
    ));
    let options = options
        .into_iter()
        .map(|(kind, id, label)| v1::PermissionOption::new(id, label, kind))
        .collect();
    let request = v1::RequestPermissionRequest::new(session.clone(), update, options);
    let answer = tokio::select! {
        answer = cx.send_request(request).block_task() => answer,
        () = cancel.cancelled() => return PermissionReply::Cancelled,
    };
    match answer.map(|a| a.outcome) {
        Ok(v1::RequestPermissionOutcome::Selected(chosen)) => {
            match chosen.option_id.to_string().as_str() {
                "allow_once" => PermissionReply::AllowOnce,
                // Only honoured when offered: a client can't widen what the agent allows.
                "allow_always" if can_always => PermissionReply::AllowAlways,
                "reject_always" => PermissionReply::RejectAlways,
                _ => PermissionReply::RejectOnce,
            }
        }
        Ok(_) => PermissionReply::Cancelled,
        Err(_) => PermissionReply::RejectOnce,
    }
}

fn tool_kind(name: &str) -> v1::ToolKind {
    match name {
        "Read" => v1::ToolKind::Read,
        "Write" | "Edit" => v1::ToolKind::Edit,
        "Grep" | "Glob" => v1::ToolKind::Search,
        "Bash" => v1::ToolKind::Execute,
        _ => v1::ToolKind::Other,
    }
}

fn chunk(text: String) -> v1::ContentChunk {
    v1::ContentChunk::new(text.into())
}

/// The `AgentEvent` to `session/update` table in SPEC §11. Permission requests and the turn's
/// end aren't updates, so they map to `None`.
fn update(event: AgentEvent) -> Option<v1::SessionUpdate> {
    Some(match event {
        AgentEvent::TextDelta(text) => v1::SessionUpdate::AgentMessageChunk(chunk(text)),
        AgentEvent::ThinkingDelta(text) => v1::SessionUpdate::AgentThoughtChunk(chunk(text)),
        AgentEvent::ToolStarted {
            id, name, title, ..
        } => v1::SessionUpdate::ToolCall(
            v1::ToolCall::new(id, title)
                .kind(tool_kind(&name))
                // `pending` is the default, so the wire leaves `status` out.
                .status(v1::ToolCallStatus::Pending),
        ),
        AgentEvent::ToolFinished { id, outcome } => {
            let (status, content) = match outcome {
                ToolOutcome::Completed(output) => {
                    let status = if output.is_error {
                        v1::ToolCallStatus::Failed
                    } else {
                        v1::ToolCallStatus::Completed
                    };
                    (status, vec![output.content.into()])
                }
                ToolOutcome::Cancelled => (v1::ToolCallStatus::Failed, vec!["Cancelled.".into()]),
            };
            let fields = v1::ToolCallUpdateFields::new()
                .status(status)
                .content(content);
            v1::SessionUpdate::ToolCallUpdate(v1::ToolCallUpdate::new(id, fields))
        }
        AgentEvent::Usage {
            used_tokens,
            context_size,
            ..
        } => v1::SessionUpdate::UsageUpdate(v1::UsageUpdate::new(
            used_tokens.into(),
            context_size.into(),
        )),
        // shortcut: notices always fall back to a message chunk and compaction isn't shown;
        // switch on the client's `session.notices` and `session.compaction` in T5.3.
        AgentEvent::Notice { title, .. } => {
            v1::SessionUpdate::AgentMessageChunk(chunk(format!("\n{title}\n")))
        }
        AgentEvent::Compaction(_)
        | AgentEvent::PermissionRequest { .. }
        | AgentEvent::TurnEnded(_) => {
            return None;
        }
    })
}

fn replay(message: Message) -> Option<v1::SessionUpdate> {
    match message {
        Message::User { content } => Some(v1::SessionUpdate::UserMessageChunk(chunk(content))),
        Message::Assistant {
            content: Some(text),
            ..
        } => Some(v1::SessionUpdate::AgentMessageChunk(chunk(text))),
        _ => None,
    }
}

/// `None` for a turn that failed rather than stopped: the client gets an error instead.
fn stop_reason(stop: StopReason) -> Option<v1::StopReason> {
    match stop {
        StopReason::EndTurn => Some(v1::StopReason::EndTurn),
        StopReason::MaxTokens => Some(v1::StopReason::MaxTokens),
        StopReason::MaxTurnRequests => Some(v1::StopReason::MaxTurnRequests),
        StopReason::Cancelled => Some(v1::StopReason::Cancelled),
        StopReason::LoopDetected | StopReason::ProviderError | StopReason::LogFailed => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::ProtocolVersion;
    use harness_core::agent::AgentConfig;
    use harness_core::provider::Provider;
    use harness_core::tools::{ToolContext, ToolOutput};
    use serde_json::{Value, json, to_value};
    use std::path::Path;
    use std::time::Duration;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sse(chunks: &[Value]) -> String {
        let body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
        body + "data: [DONE]\n\n"
    }

    /// A fake LM Studio that writes `ok.txt` through `Write`, then says "done".
    async fn fake_server() -> MockServer {
        fake_server_calling("Write", json!({"file_path": "ok.txt", "content": "x"})).await
    }

    /// A fake LM Studio that calls `name` with `args`, then says "done".
    async fn fake_server_calling(name: &str, input: Value) -> MockServer {
        let server = MockServer::start().await;
        let args = input.to_string();
        let write = sse(&[
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1",
                "function": {"name": name, "arguments": args}}]}, "finish_reason": null}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
        ]);
        let done = sse(&[
            json!({"choices": [{"delta": {"content": "done"}, "finish_reason": null}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        ]);
        for body in [write, done] {
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
                .up_to_n_times(1)
                .mount(&server)
                .await;
        }
        server
    }

    fn harness(server: &MockServer, dir: &Path) -> impl ConnectTo<Client> {
        let (url, workspace) = (server.uri(), dir.to_owned());
        let new_agent: NewAgent = Box::new(move || {
            let config = AgentConfig {
                model: "m".into(),
                context_size: 100_000,
                system_prompt: "sys".into(),
                agents_md: None,
                mask_keep_turns: 10,
                max_turn_requests: 10,
                profile: harness_core::agent::profile::ModelProfile::default(),
                seed: None,
                policy: harness_core::permissions::Policy::default(),
                masking: true,
            };
            let tools = Arc::new(ToolContext::new(&workspace)?);
            let provider = Arc::new(Provider::new(&url, None)?);
            Ok(Agent::new(provider, tools, config))
        });
        agent(new_agent, dir.join("sessions"))
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("harness-acp-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    type Log = Arc<Mutex<Vec<String>>>;

    fn push(log: &Log, entry: String) {
        log.lock().unwrap().push(entry);
    }

    /// Runs one prompt with a client that answers the permission request with `answer`, or
    /// cancels the turn there, and returns the session id and what the client saw, in order.
    async fn prompt_once(dir: &Path, answer: &'static str) -> (String, Vec<String>) {
        let server = fake_server().await;
        let log = Log::default();
        let (on_ask, on_update) = (log.clone(), log.clone());
        let mut id = String::new();
        let run = Client
            .v1()
            .on_receive_request(
                async move |req: v1::RequestPermissionRequest, responder, cx| {
                    let options: Vec<_> = req
                        .options
                        .iter()
                        .map(|o| o.option_id.to_string())
                        .collect();
                    push(&on_ask, format!("permission {}", options.join(",")));
                    let outcome = if answer == "cancel" {
                        cx.send_notification(v1::CancelNotification::new(req.session_id))?;
                        v1::RequestPermissionOutcome::Cancelled
                    } else {
                        let chosen = v1::SelectedPermissionOutcome::new(answer);
                        v1::RequestPermissionOutcome::Selected(chosen)
                    };
                    responder.respond(v1::RequestPermissionResponse::new(outcome))
                },
                on_receive_request!(),
            )
            .on_receive_notification(
                async move |note: v1::SessionNotification, _cx| {
                    let update = to_value(&note.update).unwrap();
                    let status = update["status"].as_str().map(|s| format!(" {s}"));
                    let kind = update["sessionUpdate"].as_str().unwrap();
                    push(&on_update, format!("{kind}{}", status.unwrap_or_default()));
                    Ok(())
                },
                on_receive_notification!(),
            )
            .connect_with(harness(&server, dir), async |cx| {
                let init = v1::InitializeRequest::new(ProtocolVersion::V1);
                cx.send_request(init).block_task().await?;
                let session = v1::NewSessionRequest::new(dir);
                let session = cx.send_request(session).block_task().await?;
                id = session.session_id.to_string();
                let prompt = v1::PromptRequest::new(session.session_id, vec!["go".into()]);
                let reply = cx.send_request(prompt).block_task().await?;
                push(&log, format!("{:?}", reply.stop_reason));
                Ok(())
            });
        tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .unwrap()
            .unwrap();
        let seen = log.lock().unwrap().clone();
        (id, seen)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_allowed_tool_call_round_trips_through_the_client() {
        let dir = temp_dir("allow");
        let (_, seen) = prompt_once(&dir, "allow_once").await;
        assert_eq!(
            seen,
            [
                "tool_call",
                "permission allow_once,allow_always,reject_once,reject_always",
                "tool_call_update completed",
                "agent_message_chunk",
                "EndTurn",
            ]
        );
        assert_eq!(std::fs::read_to_string(dir.join("ok.txt")).unwrap(), "x");
    }

    /// Runs one `Write` prompt and answers its permission request with `answer`. Returns the tool
    /// call the client was asked about, and the requests the model received, in order.
    async fn review_write(dir: &Path, answer: &'static str) -> (Value, Vec<Value>) {
        let server = fake_server().await;
        let asked = Arc::new(Mutex::new(Value::Null));
        let on_ask = asked.clone();
        let run = Client
            .v1()
            .on_receive_request(
                async move |req: v1::RequestPermissionRequest, responder, _cx| {
                    *on_ask.lock().unwrap() = to_value(&req.tool_call).unwrap();
                    let chosen = v1::SelectedPermissionOutcome::new(answer);
                    let outcome = v1::RequestPermissionOutcome::Selected(chosen);
                    responder.respond(v1::RequestPermissionResponse::new(outcome))
                },
                on_receive_request!(),
            )
            .connect_with(harness(&server, dir), async |cx| {
                let init = v1::InitializeRequest::new(ProtocolVersion::V1);
                cx.send_request(init).block_task().await?;
                let session = v1::NewSessionRequest::new(dir);
                let session = cx.send_request(session).block_task().await?;
                let prompt = v1::PromptRequest::new(session.session_id, vec!["go".into()]);
                cx.send_request(prompt).block_task().await?;
                Ok(())
            });
        tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .unwrap()
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        let bodies = requests
            .iter()
            .map(|request| request.body_json::<Value>().unwrap())
            .collect();
        let tool_call = asked.lock().unwrap().clone();
        (tool_call, bodies)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_permission_request_carries_the_proposed_file_content() {
        let dir = temp_dir("diff");
        let (tool_call, _) = review_write(&dir, "reject_once").await;
        assert_eq!(tool_call["title"], "Write");
        let diff = &tool_call["content"][0];
        assert_eq!(diff["type"], "diff");
        assert_eq!(diff["path"], dir.join("ok.txt").to_str().unwrap());
        assert_eq!(diff["newText"], "x");
        assert!(diff.get("oldText").is_none_or(Value::is_null), "new file");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_rejected_write_leaves_the_file_unchanged_and_the_model_is_told() {
        let dir = temp_dir("reject");
        let (_, requests) = review_write(&dir, "reject_once").await;
        assert!(!dir.join("ok.txt").exists(), "rejected write must not run");
        // The second model request carries the tool result, which names the rejection.
        let told = requests[1]["messages"].to_string();
        assert!(
            told.contains("The user rejected this `Write` call"),
            "{told}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelling_sends_final_updates_before_the_cancelled_answer() {
        let dir = temp_dir("cancel");
        let (_, seen) = prompt_once(&dir, "cancel").await;
        assert_eq!(
            seen[seen.len() - 2..],
            ["tool_call_update failed", "Cancelled"]
        );
        assert!(!dir.join("ok.txt").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn loading_a_session_replays_its_conversation_first() {
        let dir = temp_dir("load");
        let (id, _) = prompt_once(&dir, "reject_once").await;
        let server = fake_server().await;
        let log = Log::default();
        let on_update = log.clone();
        let run = Client
            .v1()
            .on_receive_notification(
                async move |note: v1::SessionNotification, _cx| {
                    let update = to_value(&note.update).unwrap();
                    let text = update["content"]["text"].as_str().unwrap_or_default();
                    push(&on_update, format!("{} {text}", update["sessionUpdate"]));
                    Ok(())
                },
                on_receive_notification!(),
            )
            .connect_with(harness(&server, &dir), async |cx| {
                let init = v1::InitializeRequest::new(ProtocolVersion::V1);
                cx.send_request(init).block_task().await?;
                let load = v1::LoadSessionRequest::new(id.clone(), &dir);
                cx.send_request(load).block_task().await?;
                push(&log, "loaded".into());
                Ok(())
            });
        tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .unwrap()
            .unwrap();
        let seen = log.lock().unwrap().clone();
        assert_eq!(
            seen,
            [
                r#""user_message_chunk" go"#,
                r#""agent_message_chunk" done"#,
                "loaded"
            ]
        );
    }

    /// Has the model `Read` `a.txt` while the client holds an unsaved edit to it. Returns the
    /// tool output the model saw and the paths the client was asked to read.
    async fn read_through_client(dir: &Path, advertise: bool) -> (String, Vec<String>) {
        let server = fake_server_calling("Read", json!({"file_path": "a.txt"})).await;
        let (asked, output) = (Log::default(), Log::default());
        let (on_read, on_update) = (asked.clone(), output.clone());
        let init = serde_json::from_value::<v1::InitializeRequest>(json!({
            "protocolVersion": 1,
            "clientCapabilities": {"fs": {"readTextFile": advertise}}
        }))
        .unwrap();
        let run = Client
            .v1()
            .on_receive_request(
                async move |req: v1::ReadTextFileRequest, responder, _cx| {
                    push(&on_read, req.path.display().to_string());
                    responder.respond(v1::ReadTextFileResponse::new("unsaved buffer"))
                },
                on_receive_request!(),
            )
            .on_receive_notification(
                async move |note: v1::SessionNotification, _cx| {
                    let update = to_value(&note.update).unwrap();
                    if update["sessionUpdate"] == "tool_call_update" {
                        let text = update["content"][0]["content"]["text"].as_str();
                        push(&on_update, text.unwrap_or_default().to_owned());
                    }
                    Ok(())
                },
                on_receive_notification!(),
            )
            .connect_with(harness(&server, dir), async |cx| {
                cx.send_request(init).block_task().await?;
                let session = v1::NewSessionRequest::new(dir);
                let session = cx.send_request(session).block_task().await?;
                let prompt = v1::PromptRequest::new(session.session_id, vec!["go".into()]);
                cx.send_request(prompt).block_task().await?;
                Ok(())
            });
        tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .unwrap()
            .unwrap();
        let output = output.lock().unwrap().join("");
        let asked = asked.lock().unwrap().clone();
        (output, asked)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_returns_the_unsaved_buffer_when_the_client_serves_files() {
        let dir = temp_dir("read-buffer");
        std::fs::write(dir.join("a.txt"), "on disk\n").unwrap();
        let (output, asked) = read_through_client(&dir, true).await;
        assert!(output.contains("unsaved buffer"), "{output}");
        assert!(!output.contains("on disk"), "{output}");
        assert_eq!(asked, [dir.join("a.txt").display().to_string()]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_uses_the_disk_when_the_client_does_not_serve_files() {
        let dir = temp_dir("read-disk");
        std::fs::write(dir.join("a.txt"), "on disk\n").unwrap();
        let (output, asked) = read_through_client(&dir, false).await;
        assert!(output.contains("on disk"), "{output}");
        assert!(asked.is_empty());
    }

    #[test]
    fn agent_events_map_to_the_session_updates_in_spec_11() {
        let json = |event| to_value(update(event)).unwrap();
        assert_eq!(
            json(AgentEvent::TextDelta("hi".into())),
            json!({"sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "hi"}})
        );
        let thought = json(AgentEvent::ThinkingDelta("hm".into()));
        assert_eq!(thought["sessionUpdate"], "agent_thought_chunk");
        let started = json(AgentEvent::ToolStarted {
            id: "c1".into(),
            name: "Bash".into(),
            input: json!({}),
            title: "Run ls".into(),
        });
        assert_eq!(
            started,
            json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "Run ls",
                "kind": "execute"})
        );
        let output = ToolOutput {
            content: "boom".into(),
            is_error: true,
            structured: Value::Null,
        };
        let outcome = ToolOutcome::Completed(output);
        let finished = json(AgentEvent::ToolFinished {
            id: "c1".into(),
            outcome,
        });
        assert_eq!(finished["sessionUpdate"], "tool_call_update");
        assert_eq!(finished["status"], "failed");
        assert_eq!(finished["content"][0]["content"]["text"], "boom");
        let usage = json(AgentEvent::Usage {
            prompt_tokens: 1,
            used_tokens: 5,
            context_size: 9,
            cached_tokens: None,
        });
        assert_eq!(
            usage,
            json!({"sessionUpdate": "usage_update", "used": 5, "size": 9})
        );
        let notice = json(AgentEvent::Notice {
            severity: harness_core::agent::Severity::Warning,
            title: "careful".into(),
            detail: None,
        });
        assert_eq!(notice["sessionUpdate"], "agent_message_chunk");
        assert!(update(AgentEvent::TurnEnded(StopReason::EndTurn)).is_none());
        let limit = stop_reason(StopReason::MaxTurnRequests);
        assert_eq!(limit, Some(v1::StopReason::MaxTurnRequests));
        assert_eq!(stop_reason(StopReason::ProviderError), None);
    }
}
