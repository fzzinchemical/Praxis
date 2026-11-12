//! `harness acp`: the agent behind an editor, over ACP v1 on stdio (SPEC §11, ADR 0003).

use agent_client_protocol::v1::{ConnectionTo, schema as v1};
use agent_client_protocol::{
    Agent as AcpAgent, Client, ConnectTo, Error, on_receive_notification, on_receive_request,
};
use harness_core::agent::{Agent, AgentEvent, PermissionReply, StopReason, ToolOutcome};
use harness_core::provider::Message;
use harness_core::session::{self, SessionLog};
use harness_core::tools::EditorReader;
use harness_core::tools::FileDiff;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub type NewAgent = Box<dyn Fn() -> anyhow::Result<Agent> + Send + Sync>;

/// What a session needs from the CLI to switch models: the picker's keys and loading (SPEC §5.2).
pub trait Models: Send + Sync + 'static {
    /// The keys the picker offers.
    fn keys(&self) -> Vec<String>;
    /// The key a new session starts on.
    fn default_key(&self) -> String;
    /// Loads `key` and returns what its turns need.
    fn load(&self, key: &str) -> impl Future<Output = anyhow::Result<Loaded>> + Send;
}

/// A loaded model, as `Agent::set_model` takes it.
pub struct Loaded {
    pub instance_id: String,
    pub context_size: u32,
    pub profile: harness_core::agent::profile::ModelProfile,
}

/// A session's agent is taken out while a turn runs, so a second prompt meanwhile is refused.
struct Slot {
    agent: Option<Agent>,
    cancel: CancellationToken,
    /// The picker key of the model the session's turns use.
    model: String,
}

struct Server<M> {
    new_agent: NewAgent,
    models: M,
    sessions_dir: PathBuf,
    sessions: Mutex<HashMap<String, Slot>>,
    /// Whether the client advertised `fs.readTextFile` at `initialize`.
    client_reads: AtomicBool,
}

/// The session config option the client shows as the model picker (ACP "model" category).
const MODEL_OPTION: &str = "model";

fn model_option(current: &str, keys: &[String]) -> v1::SessionConfigOption {
    let options = keys
        .iter()
        .map(|key| v1::SessionConfigSelectOption::new(key.clone(), key.clone()))
        .collect::<Vec<_>>();
    let mut option =
        v1::SessionConfigOption::select(MODEL_OPTION, "Model", current.to_owned(), options);
    option.category = Some(v1::SessionConfigOptionCategory::Model);
    option
}

impl<M: Models> Server<M> {
    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Slot>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Creates or resumes a session log and returns its id with the messages to replay.
    fn open(
        &self,
        resume: Option<&str>,
        cx: &ConnectionTo<Client>,
    ) -> anyhow::Result<(String, Vec<Message>, String)> {
        let (log, prior) = match resume {
            Some(id) => SessionLog::resume(&self.sessions_dir, id)?,
            None => (
                SessionLog::create(&self.sessions_dir, &std::env::current_dir()?)?,
                Vec::new(),
            ),
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
        let model = self.models.default_key();
        let slot = Slot {
            agent,
            cancel,
            model: model.clone(),
        };
        self.sessions().insert(id.clone(), slot);
        Ok((id, prior, model))
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

    /// The model option as session `id` currently has it.
    fn model_option(&self, id: &str) -> Option<v1::SessionConfigOption> {
        let current = self.sessions().get(id)?.model.clone();
        Some(model_option(&current, &self.models.keys()))
    }

    fn cancel(&self, id: &str) {
        if let Some(slot) = self.sessions().get(id) {
            slot.cancel.cancel();
        }
    }
}

pub fn agent(
    new_agent: NewAgent,
    sessions_dir: PathBuf,
    models: impl Models,
) -> impl ConnectTo<Client> {
    let server = Arc::new(Server {
        new_agent,
        models,
        sessions_dir,
        sessions: Mutex::default(),
        client_reads: AtomicBool::new(false),
    });
    let (for_init, for_new, for_load, for_list, for_prompt, for_cancel, for_set) = (
        server.clone(),
        server.clone(),
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
                let list = v1::SessionListCapabilities::new();
                let sessions = v1::SessionCapabilities::new().list(list);
                let capabilities = v1::AgentCapabilities::new()
                    .load_session(true)
                    .session_capabilities(sessions);
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
                Ok((id, _, _)) => {
                    let options = for_new.model_option(&id).map(|option| vec![option]);
                    responder.respond(v1::NewSessionResponse::new(id).config_options(options))
                }
                Err(e) => responder.respond_with_internal_error(format!("{e:#}")),
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: v1::LoadSessionRequest, responder, cx| {
                let (prior, option) = match for_load.open(Some(&req.session_id.to_string()), &cx) {
                    Ok((id, prior, _)) => (prior, for_load.model_option(&id)),
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
                let options = option.map(|option| vec![option]);
                responder.respond(v1::LoadSessionResponse::new().config_options(options))
            },
            on_receive_request!(),
        )
        // shortcut: reads every log's header on the dispatch loop, fine for a few hundred sessions.
        .on_receive_request(
            async move |req: v1::ListSessionsRequest, responder, _cx| {
                let Some(cwd) = req.cwd else {
                    let error = Error::invalid_params().data("session/list needs a cwd");
                    return responder.respond_with_error(error);
                };
                match session::list(&for_list.sessions_dir, &cwd) {
                    Ok(infos) => {
                        let sessions = infos
                            .into_iter()
                            .map(|info| {
                                let title = Some(info.title).filter(|t| !t.is_empty());
                                v1::SessionInfo::new(info.id, cwd.clone())
                                    .title(title)
                                    .updated_at(iso8601(info.last_active))
                            })
                            .collect();
                        responder.respond(v1::ListSessionsResponse::new(sessions))
                    }
                    Err(e) => responder.respond_with_internal_error(format!("{e:#}")),
                }
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
                let prompt = prompt_text(&req.prompt);
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
        .on_receive_request(
            async move |req: v1::SetSessionConfigOptionRequest, responder, cx| {
                let id = req.session_id.to_string();
                let v1::SessionConfigOptionValue::ValueId { value } = &req.value else {
                    return responder
                        .respond_with_error(Error::invalid_params().data("not a choice"));
                };
                let key = value.to_string();
                if req.config_id.to_string() != MODEL_OPTION
                    || !for_set.models.keys().contains(&key)
                {
                    let message = format!("no model `{key}`");
                    return responder.respond_with_error(Error::invalid_params().data(message));
                }
                let (mut agent, _) = match for_set.begin_turn(&id) {
                    Ok(turn) => turn,
                    Err(e) => return responder.respond_with_error(e),
                };
                let server = for_set.clone();
                // Loading can take minutes, so it runs off the dispatch loop, as prompts do.
                cx.clone().spawn(async move {
                    match server.models.load(&key).await {
                        Ok(loaded) => {
                            agent.set_model(
                                loaded.instance_id,
                                loaded.context_size,
                                loaded.profile,
                            );
                            if let Some(slot) = server.sessions().get_mut(&id) {
                                slot.model = key;
                            }
                            server.end_turn(&id, agent);
                            let options = server.model_option(&id).into_iter().collect();
                            responder.respond(v1::SetSessionConfigOptionResponse::new(options))
                        }
                        Err(e) => {
                            server.end_turn(&id, agent);
                            responder.respond_with_internal_error(format!("{e:#}"))
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
/// The model's prompt: text blocks as they are, editor context as text it can read. Images and
/// audio are dropped.
// shortcut: a resource link carries its path, not its contents; the model reads the file with Read.
fn prompt_text(blocks: &[v1::ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            v1::ContentBlock::Text(text) => Some(text.text.clone()),
            v1::ContentBlock::ResourceLink(link) => Some(format!(
                "Attached file: {}",
                link.uri.trim_start_matches("file://")
            )),
            v1::ContentBlock::Resource(embedded) => match &embedded.resource {
                v1::EmbeddedResourceResource::TextResourceContents(text) => Some(format!(
                    "Selection from {}:\n```\n{}\n```",
                    text.uri.trim_start_matches("file://"),
                    text.text
                )),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

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

/// ISO 8601 UTC for `secs` since the Unix epoch, the format ACP's `updated_at` uses.
fn iso8601(secs: u64) -> String {
    // Howard Hinnant's civil_from_days, so no date crate is needed.
    let days = (secs / 86_400) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let rem = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
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

    /// Two models; loading one just names its instance after its key.
    struct FakeModels;

    impl Models for FakeModels {
        fn keys(&self) -> Vec<String> {
            vec!["m".into(), "other".into()]
        }

        fn default_key(&self) -> String {
            "m".into()
        }

        async fn load(&self, key: &str) -> anyhow::Result<Loaded> {
            Ok(Loaded {
                instance_id: key.into(),
                context_size: 100_000,
                profile: harness_core::agent::profile::ModelProfile::default(),
            })
        }
    }

    fn harness(server: &MockServer, dir: &Path) -> impl ConnectTo<Client> {
        let (url, workspace) = (server.uri(), dir.to_owned());
        let new_agent: NewAgent = Box::new(move || {
            let config = AgentConfig {
                model: "m".into(),
                context_size: 100_000,
                system_prompt: "sys".into(),
                memory: Vec::new(),
                scoped_rules: Vec::new(),
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
        agent(new_agent, dir.join("sessions"), FakeModels)
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

    #[tokio::test(flavor = "multi_thread")]
    async fn the_session_offers_its_models_and_starts_on_the_default() {
        let dir = temp_dir("models-list");
        let server = fake_server().await;
        let option = Client
            .v1()
            .connect_with(harness(&server, &dir), async |cx| {
                let init = v1::InitializeRequest::new(ProtocolVersion::V1);
                cx.send_request(init).block_task().await?;
                let session = v1::NewSessionRequest::new(&dir);
                let session = cx.send_request(session).block_task().await?;
                Ok(to_value(&session.config_options).unwrap())
            })
            .await
            .unwrap();
        assert_eq!(option[0]["id"], "model");
        assert_eq!(option[0]["category"], "model");
        assert_eq!(option[0]["currentValue"], "m");
        assert_eq!(option[0]["options"][1]["value"], "other");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn switching_the_model_sends_the_next_turn_to_it() {
        let dir = temp_dir("models-switch");
        let server = MockServer::start().await;
        let done = sse(&[
            json!({"choices": [{"delta": {"content": "done"}, "finish_reason": null}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        ]);
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(done, "text/event-stream"))
            .mount(&server)
            .await;
        let switched = Client
            .v1()
            .connect_with(harness(&server, &dir), async |cx| {
                let init = v1::InitializeRequest::new(ProtocolVersion::V1);
                cx.send_request(init).block_task().await?;
                let session = v1::NewSessionRequest::new(&dir);
                let session = cx.send_request(session).block_task().await?;
                let set = v1::SetSessionConfigOptionRequest::new(
                    session.session_id.clone(),
                    "model",
                    "other",
                );
                let reply = cx.send_request(set).block_task().await?;
                let prompt = v1::PromptRequest::new(session.session_id, vec!["go".into()]);
                cx.send_request(prompt).block_task().await?;
                Ok(to_value(&reply.config_options).unwrap())
            })
            .await
            .unwrap();
        assert_eq!(switched[0]["currentValue"], "other");
        let requests = server.received_requests().await.unwrap();
        let body: Value = requests[0].body_json().unwrap();
        assert_eq!(body["model"], "other");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unknown_model_is_refused() {
        let dir = temp_dir("models-fail");
        let server = fake_server().await;
        let refused = Client
            .v1()
            .connect_with(harness(&server, &dir), async |cx| {
                let init = v1::InitializeRequest::new(ProtocolVersion::V1);
                cx.send_request(init).block_task().await?;
                let session = v1::NewSessionRequest::new(&dir);
                let session = cx.send_request(session).block_task().await?;
                let set =
                    v1::SetSessionConfigOptionRequest::new(session.session_id, "model", "nope");
                Ok(cx.send_request(set).block_task().await.is_err())
            })
            .await
            .unwrap();
        assert!(refused, "an unknown model is refused");
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

    /// Writes one log per workspace, each with a first prompt, and returns the sessions dir and
    /// the id of the log for `ws_a`.
    fn two_workspaces(dir: &Path) -> (PathBuf, String) {
        let (ws_a, ws_b) = (dir.join("a"), dir.join("b"));
        for ws in [&ws_a, &ws_b, &dir.join("empty")] {
            std::fs::create_dir_all(ws).unwrap();
        }
        let sessions = dir.join("sessions");
        let mut mine = SessionLog::create(&sessions, &ws_a).unwrap();
        let id = mine.id().to_owned();
        mine.append(&Message::User {
            content: "hello a".into(),
        })
        .unwrap();
        let mut theirs = SessionLog::create(&sessions, &ws_b).unwrap();
        theirs
            .append(&Message::User {
                content: "hello b".into(),
            })
            .unwrap();
        (sessions, id)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn session_list_returns_only_the_given_workspaces_sessions() {
        let dir = temp_dir("list");
        let (_, mine) = two_workspaces(&dir);
        let server = fake_server().await;
        let run = Client
            .v1()
            .connect_with(harness(&server, &dir), async |cx| {
                let init = v1::InitializeRequest::new(ProtocolVersion::V1);
                cx.send_request(init).block_task().await?;
                let list = v1::ListSessionsRequest::new().cwd(dir.join("a"));
                let listed = cx.send_request(list).block_task().await?;
                assert_eq!(listed.sessions.len(), 1);
                let info = &listed.sessions[0];
                assert_eq!(info.session_id.to_string(), mine);
                assert_eq!(info.title.as_deref(), Some("hello a"));
                assert_eq!(
                    info.updated_at.as_deref().map(|t| t.ends_with('Z')),
                    Some(true)
                );
                let other = v1::ListSessionsRequest::new().cwd(dir.join("empty"));
                let none = cx.send_request(other).block_task().await?;
                assert!(none.sessions.is_empty());
                Ok(())
            });
        tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn loading_a_listed_session_replays_it_into_a_fresh_view() {
        let dir = temp_dir("list-load");
        two_workspaces(&dir);
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
                let list = v1::ListSessionsRequest::new().cwd(dir.join("a"));
                let listed = cx.send_request(list).block_task().await?;
                let id = listed.sessions[0].session_id.clone();
                cx.send_request(v1::LoadSessionRequest::new(id, &dir))
                    .block_task()
                    .await?;
                push(&log, "loaded".into());
                Ok(())
            });
        tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            *log.lock().unwrap(),
            [r#""user_message_chunk" hello a"#, "loaded"]
        );
    }

    #[test]
    fn updated_at_is_utc_iso_8601() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(1_700_000_000), "2023-11-14T22:13:20Z");
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

    #[tokio::test(flavor = "multi_thread")]
    async fn attached_editor_context_reaches_the_model_as_text() {
        let dir = temp_dir("context");
        let server = MockServer::start().await;
        let done = sse(&[
            json!({"choices": [{"delta": {"content": "done"}, "finish_reason": null}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        ]);
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(done, "text/event-stream"))
            .mount(&server)
            .await;
        let file = dir.join("a.rs").display().to_string();
        let blocks = vec![
            v1::ContentBlock::from("go".to_owned()),
            v1::ContentBlock::ResourceLink(v1::ResourceLink::new("a.rs", format!("file://{file}"))),
            v1::ContentBlock::Resource(v1::EmbeddedResource::new(
                v1::EmbeddedResourceResource::TextResourceContents(v1::TextResourceContents::new(
                    "let x = 1;",
                    format!("file://{file}#L3-3"),
                )),
            )),
        ];
        let run = Client
            .v1()
            .connect_with(harness(&server, &dir), async |cx| {
                cx.send_request(v1::InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let session = cx
                    .send_request(v1::NewSessionRequest::new(&dir))
                    .block_task()
                    .await?;
                let prompt = v1::PromptRequest::new(session.session_id, blocks);
                cx.send_request(prompt).block_task().await?;
                Ok(())
            });
        tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .unwrap()
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        let body = requests[0].body_json::<Value>().unwrap().to_string();
        assert!(body.contains(&format!("Attached file: {file}")), "{body}");
        assert!(
            body.contains(&format!("Selection from {file}#L3-3")),
            "{body}"
        );
        assert!(body.contains("let x = 1;"), "{body}");
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
