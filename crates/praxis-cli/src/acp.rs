//! `praxis acp`: the agent behind an editor, over ACP v1 on stdio (SPEC §11, ADR 0003).

use agent_client_protocol::v1::{ConnectionTo, schema as v1};
use agent_client_protocol::{
    Agent as AcpAgent, Client, ConnectTo, Error, UntypedMessage, on_receive_notification,
    on_receive_request,
};
use praxis_core::agent::{
    Agent, AgentEvent, PermissionReply, StopReason, SubagentStops, ToolCallId, ToolOutcome, title,
};
use praxis_core::mcp::ServerStatus;
use praxis_core::permissions::Mode;
use praxis_core::plugin::Skill;
use praxis_core::provider::{MEMORY_MARK, Message, ToolCall};
use praxis_core::session::{self, Entry, SessionLog};
use praxis_core::tools::{DiagnosticsReader, EditorReader};
use praxis_core::tools::{FileDiff, ToolOutput};
use praxis_core::trust::TrustStore;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinError;
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
    pub profile: praxis_core::agent::profile::ModelProfile,
}

/// A session's agent is taken out while a turn runs, so a second prompt meanwhile is refused.
struct Slot {
    agent: Option<Agent>,
    cancel: CancellationToken,
    /// Stops one running subagent, reachable while a turn holds the agent.
    subagents: SubagentStops,
    /// The picker key of the model the session's turns use.
    model: String,
    /// The permission mode the session's turns use; session-only, so a new session starts on the config's.
    mode: Mode,
    /// The session's MCP servers, as connected when it opened. Kept here so listing them never waits for a turn.
    mcp: Vec<ServerStatus>,
    /// Servers the session turned off; pushed into the agent when a turn starts.
    mcp_off: BTreeSet<String>,
}

struct Server<M> {
    new_agent: NewAgent,
    models: M,
    sessions_dir: PathBuf,
    /// Plugin and standalone commands announced after each `session/new` and `session/load`.
    commands: Vec<Skill>,
    /// Where a sidebar approval of an MCP server is stored (SPEC §8). `None` refuses approvals.
    trust: Option<TrustStore>,
    sessions: Mutex<HashMap<String, Slot>>,
    /// Whether the client advertised `fs.readTextFile` at `initialize`.
    client_reads: AtomicBool,
    /// Whether the client advertised `_meta.praxis.diagnostics` at `initialize` (SPEC §11).
    client_diagnostics: AtomicBool,
}

/// Prompt text that makes the test build panic its turn task, where no tool can catch it.
#[cfg(test)]
const CRASH_PROMPT: &str = "crash this turn";
/// Extension notification (`_`-prefixed, SPEC §11) that stops one subagent: `sessionId`, `toolCallId`.
const STOP_SUBAGENT: &str = "_praxis/cancel_subagent";
/// Extension request the agent sends after an edit: `sessionId`, `path`. Answered with `problems`.
const DIAGNOSTICS: &str = "_praxis/diagnostics";
/// Extension request the panel sends for "Rewind to here": `sessionId`, `turn`. Answered with `turn`.
const REWIND: &str = "_praxis/rewind";
/// Extension request the panel sends for "Revert file": `sessionId`, `path`. Answered with `path`.
const REVERT: &str = "_praxis/revert_file";
/// Extension request listing the session's MCP servers: `sessionId`. Answered with `servers`.
const MCP_SERVERS: &str = "_praxis/mcp_servers";
/// Extension request that turns one MCP server on or off for the session: `sessionId`, `name`,
/// `enabled`. Applies from the session's next turn (SPEC §13).
const SET_MCP_ENABLED: &str = "_praxis/set_mcp_enabled";
/// Extension request for "Approve" in the MCP view: `sessionId`, `name`. Stores the user's approval
/// of that server and connects it for the session. Answered with `name` and `notices` (SPEC §13).
const APPROVE_MCP: &str = "_praxis/approve_mcp_server";
/// How long an edit waits for the client's diagnostics; a silent client must not hang the tool.
const DIAGNOSTICS_WAIT: Duration = Duration::from_secs(5);

/// The session config option the client shows as the model picker (ACP "model" category).
const MODEL_OPTION: &str = "model";

/// The session config option the client shows as the permission mode picker (ACP "mode" category).
const MODE_OPTION: &str = "mode";

/// The picker key and label of each mode; the key is what the client sends back.
fn mode_entry(mode: Mode) -> (&'static str, &'static str) {
    match mode {
        Mode::ReadOnly => ("read-only", "Read-only"),
        Mode::Ask => ("ask", "Ask"),
        Mode::AutoEdit => ("auto-edit", "Auto-edit"),
        Mode::FullAccess => ("full-access", "Full access"),
    }
}

fn mode_from_key(key: &str) -> Option<Mode> {
    [Mode::ReadOnly, Mode::Ask, Mode::AutoEdit, Mode::FullAccess]
        .into_iter()
        .find(|mode| mode_entry(*mode).0 == key)
}

fn mode_option(current: Mode) -> v1::SessionConfigOption {
    let options = [Mode::ReadOnly, Mode::Ask, Mode::AutoEdit, Mode::FullAccess]
        .map(mode_entry)
        .map(|(key, label)| v1::SessionConfigSelectOption::new(key, label));
    let mut option = v1::SessionConfigOption::select(
        MODE_OPTION,
        "Permissions",
        mode_entry(current).0,
        options.to_vec(),
    );
    option.category = Some(v1::SessionConfigOptionCategory::Mode);
    option
}

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
        client_cwd: &Path,
        cx: &ConnectionTo<Client>,
    ) -> anyhow::Result<(String, Vec<Entry>, String)> {
        let (log, prior) = match resume {
            Some(id) => {
                // The old slot holds the log's lock, so release it before reopening the file.
                if let Some(old) = self.sessions().remove(id) {
                    old.cancel.cancel();
                }
                SessionLog::resume_entries(&self.sessions_dir, id)?
            }
            None => (
                SessionLog::create(&self.sessions_dir, &std::env::current_dir()?)?,
                Vec::new(),
            ),
        };
        let id = log.id().to_owned();
        let agent = (self.new_agent)()?;
        let mode = agent.permission_mode();
        agent
            .tools()
            .checkpoint_into(self.sessions_dir.join(&id).join("checkpoints"));
        if self.client_reads.load(Ordering::Relaxed) {
            agent.tools().read_from_editor(editor_reader(
                cx.clone(),
                id.clone(),
                agent.tools().workspace().to_path_buf(),
                client_cwd.to_path_buf(),
            ));
        }
        if self.client_diagnostics.load(Ordering::Relaxed) {
            agent.tools().report_diagnostics(diagnostics_reader(
                cx.clone(),
                id.clone(),
                agent.tools().workspace().to_path_buf(),
                client_cwd.to_path_buf(),
            ));
        }
        let main = prior
            .iter()
            .filter(|e| e.subagent_of.is_none())
            .map(|e| e.message.clone())
            .collect();
        let subagents = agent.subagent_stops();
        let mcp = agent.mcp_servers();
        let agent = Some(agent.with_session(log, main));
        let cancel = CancellationToken::new();
        let model = self.models.default_key();
        let slot = Slot {
            agent,
            cancel,
            subagents,
            model: model.clone(),
            mode,
            mcp,
            mcp_off: BTreeSet::new(),
        };
        self.sessions().insert(id.clone(), slot);
        Ok((id, prior, model))
    }

    fn begin_turn(&self, id: &str) -> Result<(Agent, CancellationToken), Error> {
        let mut sessions = self.sessions();
        let slot = sessions
            .get_mut(id)
            .ok_or_else(|| Error::invalid_params().data(format!("unknown session `{id}`")))?;
        let mut agent = slot
            .agent
            .take()
            .ok_or_else(|| Error::invalid_params().data("a prompt is already running"))?;
        agent.set_mcp_disabled(slot.mcp_off.clone());
        slot.cancel = CancellationToken::new();
        Ok((agent, slot.cancel.clone()))
    }

    /// The session's MCP servers and whether each is on. Answered from the slot, so it also
    /// works while a turn runs.
    fn mcp_servers(&self, id: &str) -> Result<Value, Error> {
        let sessions = self.sessions();
        let slot = sessions
            .get(id)
            .ok_or_else(|| Error::invalid_params().data(format!("unknown session `{id}`")))?;
        let servers: Vec<Value> = slot
            .mcp
            .iter()
            .map(|s| {
                let mut row = json!({
                    "name": s.name,
                    "source": s.source.as_str(),
                    "status": s.status.as_str(),
                    "tools": s.tools,
                    "enabled": !slot.mcp_off.contains(&s.name),
                });
                // Only servers waiting for approval carry what the approval would run (SPEC §13).
                if let Some(command) = &s.command {
                    row["command"] = json!(command);
                }
                row
            })
            .collect();
        Ok(json!({"servers": servers}))
    }

    /// Turns one MCP server on or off for session `id`, from its next turn (SPEC §13).
    fn set_mcp_enabled(&self, id: &str, name: &str, enabled: bool) -> Result<(), Error> {
        let mut sessions = self.sessions();
        let slot = sessions
            .get_mut(id)
            .ok_or_else(|| Error::invalid_params().data(format!("unknown session `{id}`")))?;
        if !slot.mcp.iter().any(|s| s.name == name) {
            return Err(Error::invalid_params().data(format!("no MCP server named `{name}`")));
        }
        if enabled {
            slot.mcp_off.remove(name);
        } else {
            slot.mcp_off.insert(name.to_owned());
        }
        Ok(())
    }

    /// Approves one MCP server the user chose in the view and connects it for session `id`. Like
    /// a revert, it takes the agent, so it is refused at once while a turn runs (SPEC §13). The
    /// approval is stored before the connection is tried, so a failed connect stays approved.
    async fn approve_mcp(&self, id: &str, name: &str) -> Result<Vec<String>, Error> {
        let (mut agent, _) = self.begin_turn(id)?;
        let outcome = match &self.trust {
            Some(trust) => agent
                .approve_mcp(name, trust)
                .await
                .map_err(|e| Error::invalid_params().data(e)),
            None => Err(Error::invalid_params()
                .data("approving an MCP server needs a home or data directory to store it in")),
        };
        let mcp = agent.mcp_servers();
        self.end_turn(id, agent);
        if let Some(slot) = self.sessions().get_mut(id) {
            slot.mcp = mcp;
        }
        outcome
    }

    fn end_turn(&self, id: &str, agent: Agent) {
        if let Some(slot) = self.sessions().get_mut(id) {
            slot.agent = Some(agent);
        }
    }

    /// Puts back the agent a finished turn returned. A panicked turn returns none, so its session
    /// is dropped: the next prompt then fails as unknown, and the session can be loaded again.
    fn settle_turn(&self, id: &str, joined: Result<Agent, JoinError>) -> Result<(), Error> {
        match joined {
            Ok(agent) => {
                self.end_turn(id, agent);
                Ok(())
            }
            Err(e) => {
                self.sessions().remove(id);
                Err(Error::internal_error().data(format!("the turn crashed: {e}")))
            }
        }
    }

    /// The config options as session `id` currently has them; empty for an unknown session.
    fn config_options(&self, id: &str) -> Vec<v1::SessionConfigOption> {
        let Some((model, mode)) = self.sessions().get(id).map(|s| (s.model.clone(), s.mode)) else {
            return Vec::new();
        };
        vec![model_option(&model, &self.models.keys()), mode_option(mode)]
    }

    fn cancel(&self, id: &str) {
        if let Some(slot) = self.sessions().get(id) {
            slot.cancel.cancel();
        }
    }

    /// Stops the subagent running under `tool_call` in session `id`. One that already finished
    /// is no longer registered, so the request is then a no-op.
    fn stop_subagent(&self, id: &str, tool_call: &str) {
        if let Some(slot) = self.sessions().get(id) {
            slot.subagents.stop(tool_call);
        }
    }

    /// Drops the session's slot. A running turn is cancelled, and its agent is dropped when it
    /// ends, since `settle_turn` then finds no slot to return it to.
    fn close(&self, id: &str) -> Result<(), Error> {
        let slot = self
            .sessions()
            .remove(id)
            .ok_or_else(|| Error::invalid_params().data(format!("unknown session `{id}`")))?;
        slot.cancel.cancel();
        Ok(())
    }
}

/// Hides `user-invocable: false` skills. The source is the plugin (the part of `<plugin>:<name>`)
/// or, for a standalone skill, the skills directory it sits in, and goes in `_meta` so the panel can group by it.
/// None when nothing is user-invocable, so a session without commands replays as it did before.
fn commands_update(
    session: impl Into<v1::SessionId>,
    commands: &[Skill],
) -> Option<v1::SessionNotification> {
    let list: Vec<_> = commands
        .iter()
        .filter(|s| s.user_invocable)
        .map(|s| {
            let source = match s.name.split_once(':') {
                Some((plugin, _)) => plugin.to_owned(),
                // A standalone skill's folder is its own; group by the skills directory holding it.
                None => s.dir.parent().unwrap_or(&s.dir).display().to_string(),
            };
            let meta = serde_json::Map::from_iter([("source".into(), Value::String(source))]);
            v1::AvailableCommand::new(&s.name, &s.description).meta(meta)
        })
        .collect();
    if list.is_empty() {
        return None;
    }
    Some(v1::SessionNotification::new(
        session,
        v1::SessionUpdate::AvailableCommandsUpdate(v1::AvailableCommandsUpdate::new(list)),
    ))
}

pub fn agent(
    new_agent: NewAgent,
    sessions_dir: PathBuf,
    models: impl Models,
    commands: Vec<Skill>,
    trust: Option<TrustStore>,
) -> impl ConnectTo<Client> {
    let server = Arc::new(Server {
        new_agent,
        models,
        sessions_dir,
        commands,
        trust,
        sessions: Mutex::default(),
        client_reads: AtomicBool::new(false),
        client_diagnostics: AtomicBool::new(false),
    });
    let (
        for_init,
        for_new,
        for_load,
        for_list,
        for_prompt,
        for_cancel,
        for_set,
        for_close,
        for_stop,
        for_rewind,
    ) = (
        server.clone(),
        server.clone(),
        server.clone(),
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
                let diagnostics = req
                    .client_capabilities
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.get("praxis"))
                    .and_then(|praxis| praxis["diagnostics"].as_bool())
                    .unwrap_or(false);
                for_init
                    .client_diagnostics
                    .store(diagnostics, Ordering::Relaxed);
                let list = v1::SessionListCapabilities::new();
                let close = v1::SessionCloseCapabilities::new();
                let sessions = v1::SessionCapabilities::new().list(list).close(close);
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
            async move |req: v1::NewSessionRequest, responder, cx| match for_new
                .open(None, &req.cwd, &cx)
            {
                Ok((id, _, _)) => {
                    if let Some(note) = commands_update(id.clone(), &for_new.commands) {
                        cx.send_notification(note)?;
                    }
                    let options = Some(for_new.config_options(&id));
                    responder.respond(v1::NewSessionResponse::new(id).config_options(options))
                }
                Err(e) => responder.respond_with_internal_error(format!("{e:#}")),
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: v1::LoadSessionRequest, responder, cx| {
                let (prior, option) =
                    match for_load.open(Some(&req.session_id.to_string()), &req.cwd, &cx) {
                        Ok((id, prior, _)) => (prior, for_load.config_options(&id)),
                        Err(e) => {
                            return responder
                                .respond_with_error(Error::invalid_params().data(e.to_string()));
                        }
                    };
                if let Some(note) = commands_update(req.session_id.clone(), &for_load.commands) {
                    cx.send_notification(note)?;
                }
                // The client rebuilds the conversation from these before the load answer.
                for update in prior.into_iter().flat_map(replay) {
                    cx.send_notification(v1::SessionNotification::new(
                        req.session_id.clone(),
                        update,
                    ))?;
                }
                responder.respond(v1::LoadSessionResponse::new().config_options(Some(option)))
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
                        // Test seam: a turn that panics on the task itself, where tools can't catch it.
                        #[cfg(test)]
                        if prompt == CRASH_PROMPT {
                            panic!("the test crashed this turn");
                        }
                        agent.run_turn(prompt, &tx, turn_cancel).await;
                        agent
                    });
                    let mut stop = StopReason::ProviderError;
                    let mut subagent_text = HashMap::new();
                    while let Some(event) = rx.recv().await {
                        // A subagent's events arrive tagged with the `Agent` call that runs it.
                        let (owner, event) = match event {
                            AgentEvent::Subagent {
                                parent,
                                kind,
                                event,
                            } => (Some((parent, kind)), *event),
                            other => (None, other),
                        };
                        match event {
                            AgentEvent::PermissionRequest {
                                id: call,
                                name,
                                input,
                                can_always,
                                diff,
                                respond,
                            } => {
                                // The title names the subagent, so the user can tell who asks.
                                let name = match &owner {
                                    Some((_, kind)) => format!("{kind}: {name}"),
                                    None => name,
                                };
                                let update = permission_update(call, name, input, diff);
                                let reply =
                                    ask(&cx, &req.session_id, update, can_always, &cancel).await;
                                // A send failure means the turn already ended, so nothing runs.
                                let _ = respond.send(reply);
                            }
                            AgentEvent::TurnEnded(reason) => stop = reason,
                            other => {
                                let sent = match &owner {
                                    Some((parent, _)) => {
                                        subagent_update(parent, other, &mut subagent_text)
                                    }
                                    None => update(other),
                                };
                                if let Some(sent) = sent
                                    && cx
                                        .send_notification(v1::SessionNotification::new(
                                            req.session_id.clone(),
                                            sent,
                                        ))
                                        .is_err()
                                {
                                    // The client is gone: stop the turn, but keep draining so the agent comes back.
                                    cancel.cancel();
                                }
                            }
                        }
                    }
                    let settled = server.settle_turn(&id, task.await);
                    if let Err(e) = settled {
                        return responder.respond_with_error(e);
                    }
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
                if req.config_id.to_string() == MODE_OPTION {
                    let Some(mode) = mode_from_key(&key) else {
                        let message = format!("no mode `{key}`");
                        return responder.respond_with_error(Error::invalid_params().data(message));
                    };
                    let (mut agent, _) = match for_set.begin_turn(&id) {
                        Ok(turn) => turn,
                        Err(e) => return responder.respond_with_error(e),
                    };
                    agent.set_mode(mode);
                    if let Some(slot) = for_set.sessions().get_mut(&id) {
                        slot.mode = mode;
                    }
                    for_set.end_turn(&id, agent);
                    let options = for_set.config_options(&id);
                    return responder.respond(v1::SetSessionConfigOptionResponse::new(options));
                }
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
                            let options = server.config_options(&id);
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
        .on_receive_notification(
            async move |note: UntypedMessage, _cx| {
                // Only the stop method is handled, so other notifications and malformed params are ignored.
                if note.method() == STOP_SUBAGENT
                    && let (Some(session), Some(tool_call)) = (
                        note.params()["sessionId"].as_str(),
                        note.params()["toolCallId"].as_str(),
                    )
                {
                    for_stop.stop_subagent(session, tool_call);
                }
                Ok(())
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            async move |req: v1::CloseSessionRequest, responder, _cx| match for_close
                .close(&req.session_id.to_string())
            {
                Ok(()) => responder.respond(v1::CloseSessionResponse::new()),
                Err(e) => responder.respond_with_error(e),
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: UntypedMessage, responder, cx| {
                // Last in the chain, so it only sees methods no handler above took.
                if req.method() == REVERT {
                    let params = req.params();
                    let (Some(session), Some(path)) =
                        (params["sessionId"].as_str(), params["path"].as_str())
                    else {
                        let error =
                            Error::invalid_params().data("revert needs a sessionId and a path");
                        return responder.respond_with_error(error);
                    };
                    // Like a rewind, a running turn holds the agent, so the revert waits for it.
                    let (agent, _) = match for_rewind.begin_turn(session) {
                        Ok(turn) => turn,
                        Err(e) => return responder.respond_with_error(e),
                    };
                    let outcome = agent.revert_file(path);
                    for_rewind.end_turn(session, agent);
                    return match outcome {
                        Ok(()) => responder.respond(json!({"path": path})),
                        Err(e) => responder
                            .respond_with_error(Error::invalid_params().data(e.to_string())),
                    };
                }
                if req.method() == MCP_SERVERS {
                    let Some(session) = req.params()["sessionId"].as_str() else {
                        let error = Error::invalid_params().data("mcp_servers needs a sessionId");
                        return responder.respond_with_error(error);
                    };
                    return match for_rewind.mcp_servers(session) {
                        Ok(list) => responder.respond(list),
                        Err(e) => responder.respond_with_error(e),
                    };
                }
                if req.method() == SET_MCP_ENABLED {
                    let params = req.params();
                    let (Some(session), Some(name), Some(enabled)) = (
                        params["sessionId"].as_str(),
                        params["name"].as_str(),
                        params["enabled"].as_bool(),
                    ) else {
                        let error = Error::invalid_params()
                            .data("set_mcp_enabled needs a sessionId, a name and enabled");
                        return responder.respond_with_error(error);
                    };
                    return match for_rewind.set_mcp_enabled(session, name, enabled) {
                        Ok(()) => responder.respond(json!({"name": name, "enabled": enabled})),
                        Err(e) => responder.respond_with_error(e),
                    };
                }
                if req.method() == APPROVE_MCP {
                    let (Some(session), Some(name)) = (
                        req.params()["sessionId"].as_str(),
                        req.params()["name"].as_str(),
                    ) else {
                        let error = Error::invalid_params()
                            .data("approve_mcp_server needs a sessionId and a name");
                        return responder.respond_with_error(error);
                    };
                    let (session, name) = (session.to_owned(), name.to_owned());
                    let server = for_rewind.clone();
                    // Connecting can take seconds, so it runs off the dispatch loop, as loads do.
                    return cx.clone().spawn(async move {
                        match server.approve_mcp(&session, &name).await {
                            Ok(notices) => {
                                responder.respond(json!({"name": name, "notices": notices}))
                            }
                            Err(e) => responder.respond_with_error(e),
                        }
                    });
                }
                if req.method() != REWIND {
                    return responder.respond_with_error(Error::method_not_found());
                }
                let params = req.params();
                let (Some(session), Some(turn)) = (
                    params["sessionId"].as_str(),
                    params["turn"]
                        .as_u64()
                        .and_then(|t| usize::try_from(t).ok()),
                ) else {
                    let error = Error::invalid_params().data("rewind needs a sessionId and a turn");
                    return responder.respond_with_error(error);
                };
                // A running turn holds the agent, so `begin_turn` refuses the rewind meanwhile.
                let (mut agent, _) = match for_rewind.begin_turn(session) {
                    Ok(turn) => turn,
                    Err(e) => return responder.respond_with_error(e),
                };
                let outcome = agent.rewind_to_turn(turn);
                for_rewind.end_turn(session, agent);
                match outcome {
                    Ok(_) => responder.respond(json!({"turn": turn})),
                    Err(e) => {
                        responder.respond_with_error(Error::invalid_params().data(e.to_string()))
                    }
                }
            },
            on_receive_request!(),
        )
}

/// Reads through the client, so `Read` sees unsaved buffers. Runs on the tool's blocking thread,
/// never in the dispatch loop, so waiting for the client's answer here cannot deadlock.
/// shortcut: a failed request falls back to the disk, which `Read` has already confined.
fn editor_reader(
    cx: ConnectionTo<Client>,
    session: String,
    workspace: PathBuf,
    client_cwd: PathBuf,
) -> EditorReader {
    EditorReader(Box::new(move |path| {
        let path = client_path(&workspace, &client_cwd, path);
        let request = v1::ReadTextFileRequest::new(session.clone(), path);
        let answer =
            tokio::runtime::Handle::current().block_on(cx.send_request(request).block_task());
        answer.ok().map(|reply| reply.content)
    }))
}

/// Asks the client for a file's error problems after an edit (SPEC §11). Runs on the tool's
/// blocking thread, like `editor_reader`, and gives up after `DIAGNOSTICS_WAIT`.
/// shortcut: a late, failed or malformed answer means no problem lines, never an edit error.
fn diagnostics_reader(
    cx: ConnectionTo<Client>,
    session: String,
    workspace: PathBuf,
    client_cwd: PathBuf,
) -> DiagnosticsReader {
    DiagnosticsReader(Box::new(move |path| {
        let path = client_path(&workspace, &client_cwd, path);
        let request =
            UntypedMessage::new(DIAGNOSTICS, json!({"sessionId": session, "path": path})).ok()?;
        let answer = tokio::runtime::Handle::current().block_on(async {
            tokio::time::timeout(DIAGNOSTICS_WAIT, cx.send_request(request).block_task()).await
        });
        let reply = answer.ok()?.ok()?;
        Some(problem_lines(&reply))
    }))
}

/// The error lines of a `_praxis/diagnostics` answer, as `line N: message`. The message is
/// editor text, so line breaks are flattened to keep one problem per line.
fn problem_lines(reply: &Value) -> Vec<String> {
    reply["problems"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|problem| {
            if problem["severity"] != "error" {
                return None;
            }
            let line = problem["line"].as_u64()?;
            let message = problem["message"].as_str()?.replace(['\r', '\n'], " ");
            Some(format!("line {line}: {message}"))
        })
        .collect()
}

/// Maps a canonical workspace path to the client's spelling of its folder, since the client looks
/// up unsaved buffers by the path it knows (a symlinked folder differs from its target).
fn client_path(workspace: &Path, client_cwd: &Path, path: &Path) -> PathBuf {
    match path.strip_prefix(workspace) {
        Ok(rest) => client_cwd.join(rest),
        Err(_) => path.to_path_buf(),
    }
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
            id,
            name,
            input,
            title,
            before,
        } => {
            let call = v1::ToolCall::new(id, title)
                .kind(tool_kind(&name))
                // `pending` is the default, so the wire leaves `status` out.
                .status(v1::ToolCallStatus::Pending);
            // The panel titles the subagent's block with its type (SPEC §11).
            let kind = input["subagent_type"].as_str().filter(|_| name == "Agent");
            // A Write or Edit names its file, so the panel can list what a subagent changed.
            let file = input["file_path"]
                .as_str()
                .filter(|_| matches!(name.as_str(), "Write" | "Edit"));
            let mut praxis = serde_json::Map::new();
            if let Some(kind) = kind {
                praxis.insert("subagent".into(), kind.into());
            }
            if let Some(file) = file {
                praxis.insert("filePath".into(), file.into());
                // Lets the editor show the diff from before the first change; see `before` on the event.
                if let Some(before) = before {
                    praxis.insert("before".into(), before.into());
                }
            }
            let meta = (!praxis.is_empty())
                .then(|| serde_json::Map::from_iter([("praxis".to_owned(), praxis.into())]));
            v1::SessionUpdate::ToolCall(call.meta(meta))
        }
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
        | AgentEvent::Subagent { .. }
        | AgentEvent::TurnEnded(_) => {
            return None;
        }
    })
}

/// A subagent's event as the parent client sees it. Its text replaces the `Agent` call's content,
/// so a plain client still sees one flat list; its tool calls keep their own ids and name the
/// `Agent` call in `_meta`, which the panel nests them under.
fn subagent_update(
    parent: &ToolCallId,
    event: AgentEvent,
    texts: &mut HashMap<ToolCallId, String>,
) -> Option<v1::SessionUpdate> {
    // The subagent's prompt size goes on its Agent call only, never the parent's context gauge.
    if let AgentEvent::Usage {
        prompt_tokens,
        context_size,
        ..
    } = event
    {
        let usage = json!({"promptTokens": prompt_tokens, "contextLength": context_size});
        let meta = json!({"praxis": {"subagentUsage": usage}})
            .as_object()
            .cloned();
        let update = v1::ToolCallUpdate::new(parent.clone(), v1::ToolCallUpdateFields::new());
        return Some(v1::SessionUpdate::ToolCallUpdate(update.meta(meta)));
    }
    if let AgentEvent::TextDelta(delta) = event {
        let text = texts.entry(parent.clone()).or_default();
        text.push_str(&delta);
        // The final reply arrives as the Agent call's result, so a last partial line is not lost.
        if !delta.contains('\n') {
            return None;
        }
        let fields = v1::ToolCallUpdateFields::new().content(vec![text.as_str().into()]);
        return Some(v1::SessionUpdate::ToolCallUpdate(v1::ToolCallUpdate::new(
            parent.clone(),
            fields,
        )));
    }
    // Merged, not replaced: the call may already carry its own `praxis` fields, such as `filePath`.
    let with_parent = |meta: Option<serde_json::Map<String, Value>>| {
        let mut meta = meta.unwrap_or_default();
        let praxis = meta.entry("praxis").or_insert_with(|| json!({}));
        praxis["parentToolCallId"] = json!(parent);
        Some(meta)
    };
    match update(event)? {
        v1::SessionUpdate::ToolCall(call) => {
            let meta = with_parent(call.meta.clone());
            Some(v1::SessionUpdate::ToolCall(call.meta(meta)))
        }
        v1::SessionUpdate::ToolCallUpdate(call) => {
            let meta = with_parent(call.meta.clone());
            Some(v1::SessionUpdate::ToolCallUpdate(call.meta(meta)))
        }
        // shortcut: a subagent's reasoning and notices are dropped rather than shown on its call; upgrade with the panel's sub-block rendering.
        _ => None,
    }
}

/// The updates a live turn would have sent for one logged entry, so a loaded chat looks the same.
fn replay(entry: Entry) -> Vec<v1::SessionUpdate> {
    let Some(parent) = entry.subagent_of else {
        return replay_message(entry.message);
    };
    // A subagent's calls replay as its live events do. Its text is left out: live, it becomes the
    // Agent call's content, and the Agent call's result already carries the reply.
    let events: Vec<AgentEvent> = match entry.message {
        Message::Assistant { tool_calls, .. } => {
            tool_calls.into_iter().map(replayed_call).collect()
        }
        Message::Tool {
            tool_call_id,
            content,
        } => vec![replayed_result(tool_call_id, content)],
        Message::System { .. } | Message::User { .. } => Vec::new(),
    };
    events
        .into_iter()
        .filter_map(|event| subagent_update(&parent, event, &mut HashMap::new()))
        .collect()
}

fn replay_message(message: Message) -> Vec<v1::SessionUpdate> {
    match message {
        // Memory injected mid-turn is not a bubble, so the panel's turn numbers match the log's.
        Message::User { content } if !content.starts_with(MEMORY_MARK) => {
            vec![v1::SessionUpdate::UserMessageChunk(chunk(content))]
        }
        Message::User { .. } => Vec::new(),
        Message::Assistant {
            content,
            tool_calls,
        } => {
            let mut updates: Vec<_> = content
                .map(|text| v1::SessionUpdate::AgentMessageChunk(chunk(text)))
                .into_iter()
                .collect();
            updates.extend(
                tool_calls
                    .into_iter()
                    .flat_map(|call| update(replayed_call(call))),
            );
            updates
        }
        Message::Tool {
            tool_call_id,
            content,
        } => update(replayed_result(tool_call_id, content))
            .into_iter()
            .collect(),
        Message::System { .. } => Vec::new(),
    }
}

fn replayed_call(call: ToolCall) -> AgentEvent {
    let input = serde_json::from_str::<Value>(&call.arguments).ok();
    let title = title(&call.name, input.as_ref());
    AgentEvent::ToolStarted {
        id: call.id,
        name: call.name,
        input: input.unwrap_or(Value::Null),
        title,
        before: None,
    }
}

// shortcut: the log keeps no error flag, so every replayed result shows as completed.
fn replayed_result(id: ToolCallId, content: String) -> AgentEvent {
    AgentEvent::ToolFinished {
        id,
        outcome: ToolOutcome::Completed(ToolOutput {
            content,
            is_error: false,
            structured: Value::Null,
        }),
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
    use praxis_core::agent::AgentConfig;
    use praxis_core::provider::{Provider, ToolCall};
    use praxis_core::tools::{ToolContext, ToolOutput};
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
        fake_server_script(&[(name, input)]).await
    }

    /// A fake LM Studio that makes each call in turn, one model request per call, then says "done".
    async fn fake_server_script(calls: &[(&str, Value)]) -> MockServer {
        let mut bodies: Vec<String> = calls
            .iter()
            .enumerate()
            .map(|(i, (name, input))| tool_call_body(&format!("c{i}"), name, input))
            .collect();
        bodies.push(text_body("done"));
        fake_server_bodies(bodies).await
    }

    /// One model reply that calls `name` with `input` under call id `id`.
    fn tool_call_body(id: &str, name: &str, input: &Value) -> String {
        sse(&[
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": id,
                "function": {"name": name, "arguments": input.to_string()}}]},
                "finish_reason": null}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
        ])
    }

    /// One model reply that says `text` and stops.
    fn text_body(text: &str) -> String {
        sse(&[
            json!({"choices": [{"delta": {"content": text}, "finish_reason": null}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        ])
    }

    /// A fake LM Studio that answers each request with the next body, in order.
    async fn fake_server_bodies(bodies: Vec<String>) -> MockServer {
        let server = MockServer::start().await;
        for body in bodies {
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
                .up_to_n_times(1)
                .mount(&server)
                .await;
        }
        // Answers T13.7's check-your-change note after a turn that wrote a file, with an empty
        // reply, so the event lists the tests expect stay as they were.
        let empty = sse(&[json!({"choices": [{"delta": {}, "finish_reason": "stop"}]})]);
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(empty, "text/event-stream"))
            .mount(&server)
            .await;
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
                profile: praxis_core::agent::profile::ModelProfile::default(),
            })
        }
    }

    fn praxis(server: &MockServer, dir: &Path) -> impl ConnectTo<Client> {
        praxis_with(server, dir, Vec::new())
    }

    fn praxis_with(
        server: &MockServer,
        dir: &Path,
        commands: Vec<Skill>,
    ) -> impl ConnectTo<Client> {
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
                profile: praxis_core::agent::profile::ModelProfile::default(),
                seed: None,
                policy: praxis_core::permissions::Policy::default(),
                masking: true,
            };
            let tools = Arc::new(ToolContext::new(&workspace)?);
            let provider = Arc::new(Provider::new(&url, None)?);
            Ok(Agent::new(provider, tools, config).with_agents(Vec::new()))
        });
        agent(new_agent, dir.join("sessions"), FakeModels, commands, None)
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("praxis-acp-{}-{name}", std::process::id()));
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
            .connect_with(praxis(&server, dir), async |cx| {
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

    /// A parent turn that delegates to `general`, which asks to write a file, then answers.
    /// Returns the session updates and the permission titles the client saw, in order.
    async fn subagent_turn(dir: &Path) -> (Vec<Value>, Vec<String>) {
        // The subagent reports 12000 prompt tokens, which must not show as the parent's.
        let reply = sse(&[
            json!({"choices": [{"delta": {"content": "found it\n"}, "finish_reason": null}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 12000, "completion_tokens": 3}}),
        ]);
        subagent_turn_replying(dir, reply).await
    }

    /// `subagent_turn` with the subagent's SSE reply given by the caller.
    async fn subagent_turn_replying(dir: &Path, reply: String) -> (Vec<Value>, Vec<String>) {
        let server = fake_server_bodies(vec![
            tool_call_body(
                "c0",
                "Agent",
                &json!({"subagent_type": "general", "prompt": "write it"}),
            ),
            tool_call_body(
                "c1",
                "Write",
                &json!({"file_path": "sub.txt", "content": "x"}),
            ),
            // The subagent wrote a file, so T13.7's note asks it once more before it ends.
            reply,
            sse(&[json!({"choices": [{"delta": {}, "finish_reason": "stop"}]})]),
            text_body("done"),
        ])
        .await;
        let (updates, asks) = (Log::default(), Log::default());
        let (on_update, on_ask) = (updates.clone(), asks.clone());
        let run = Client
            .v1()
            .on_receive_request(
                async move |req: v1::RequestPermissionRequest, responder, _cx| {
                    let title = to_value(&req).unwrap()["toolCall"]["title"].to_string();
                    push(&on_ask, title);
                    let chosen = v1::SelectedPermissionOutcome::new("allow_once");
                    let outcome = v1::RequestPermissionOutcome::Selected(chosen);
                    responder.respond(v1::RequestPermissionResponse::new(outcome))
                },
                on_receive_request!(),
            )
            .on_receive_notification(
                async move |note: v1::SessionNotification, _cx| {
                    push(&on_update, to_value(&note.update).unwrap().to_string());
                    Ok(())
                },
                on_receive_notification!(),
            )
            .connect_with(praxis(&server, dir), async |cx| {
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
        let updates = updates
            .lock()
            .unwrap()
            .iter()
            .map(|u| serde_json::from_str(u).unwrap())
            .collect();
        let asks = asks.lock().unwrap().clone();
        (updates, asks)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_subagents_calls_carry_the_parent_id_and_its_text_stays_on_the_agent_call() {
        let dir = temp_dir("subagent");
        let (updates, asks) = subagent_turn(&dir).await;
        let call = |id: &str| updates.iter().find(|u| u["toolCallId"] == id).cloned();

        let agent = call("c0").expect("the Agent call is sent");
        assert_eq!(agent["_meta"]["praxis"]["subagent"], "general");
        let child = call("c1").expect("the subagent's Write is sent");
        assert_eq!(child["_meta"]["praxis"]["parentToolCallId"], "c0");
        // The tool has resolved its input to an absolute path by the time the call starts.
        assert!(
            child["_meta"]["praxis"]["filePath"]
                .as_str()
                .is_some_and(|p| p.ends_with("/sub.txt"))
        );
        // Its text is on the Agent call, not a flat message chunk, and the parent's reply stays flat.
        let agent_text: Vec<&str> = updates
            .iter()
            .filter(|u| u["toolCallId"] == "c0")
            .filter_map(|u| u["content"][0]["content"]["text"].as_str())
            .collect();
        assert_eq!(agent_text.last(), Some(&"found it\n"), "{updates:?}");
        assert!(
            !updates
                .iter()
                .any(|u| u["sessionUpdate"] == "agent_message_chunk"
                    && u["content"]["text"] == "found it\n")
        );
        assert!(
            updates
                .iter()
                .any(|u| u["sessionUpdate"] == "agent_message_chunk"
                    && u["content"]["text"] == "done")
        );
        // The permission names the subagent.
        assert_eq!(asks, [r#""general: Write""#]);
        // The log keeps the subagent's call under the Agent call, so a reload can nest it.
        let logs = std::fs::read_dir(dir.join("sessions")).unwrap();
        let log = logs.map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap());
        let logged = log.collect::<String>();
        assert!(logged.contains(r#""subagent_of":"c0""#), "{logged}");
        assert!(!logged.contains(r#""subagent_of":"c1""#), "{logged}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_subagents_text_is_sent_per_line_not_per_delta() {
        let dir = temp_dir("subagent_lines");
        let deltas = ["one ", "two ", "three\n", "four ", "five\n", "six"];
        let mut chunks: Vec<Value> = deltas
            .iter()
            .map(|d| json!({"choices": [{"delta": {"content": d}, "finish_reason": null}]}))
            .collect();
        chunks.push(json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}));
        let (updates, _) = subagent_turn_replying(&dir, sse(&chunks)).await;

        let sent = updates
            .iter()
            // The completed update carries the Agent's result, not streamed text, so it is not counted.
            .filter(|u| {
                u["toolCallId"] == "c0" && u["content"].is_array() && u["status"] != "completed"
            })
            .count();
        assert!(sent <= 2, "{sent} text updates for six deltas: {updates:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_subagents_usage_goes_on_its_agent_call_not_the_parents_context() {
        let dir = temp_dir("subagent-usage");
        let (updates, _) = subagent_turn(&dir).await;
        let usage = updates
            .iter()
            .find(|u| u["toolCallId"] == "c0" && u["_meta"]["praxis"]["subagentUsage"].is_object())
            .expect("the Agent call carries the subagent's usage");
        assert_eq!(
            usage["_meta"]["praxis"]["subagentUsage"]["promptTokens"],
            12000
        );
        assert!(usage["_meta"]["praxis"]["subagentUsage"]["contextLength"].is_u64());
        // The parent's context gauge is unchanged by the subagent's prompt.
        assert!(
            !updates
                .iter()
                .any(|u| u["sessionUpdate"] == "usage_update" && u["used"] == 12003),
            "{updates:?}"
        );
    }

    #[tokio::test]
    async fn the_mcp_extension_requests_answer_and_refuse_bad_input() {
        let dir = temp_dir("mcp-requests");
        let server = fake_server().await;
        let run = Client.v1().connect_with(praxis(&server, &dir), async |cx| {
            let init = v1::InitializeRequest::new(ProtocolVersion::V1);
            cx.send_request(init).block_task().await?;
            let session = cx
                .send_request(v1::NewSessionRequest::new(&dir))
                .block_task()
                .await?
                .session_id
                .to_string();
            let listed = UntypedMessage::new(MCP_SERVERS, json!({"sessionId": session}))?;
            let listed = cx.send_request(listed).block_task().await?;
            let toggle = json!({"sessionId": session, "name": "nope", "enabled": false});
            let refused = cx
                .send_request(UntypedMessage::new(SET_MCP_ENABLED, toggle)?)
                .block_task()
                .await;
            let missing = UntypedMessage::new(MCP_SERVERS, json!({}))?;
            let no_session = cx.send_request(missing).block_task().await;
            Ok((listed, refused.is_err(), no_session.is_err()))
        });
        let (listed, refused, no_session) = tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(listed["servers"], json!([]));
        assert!(
            refused,
            "a toggle for an unknown server must be an error reply"
        );
        assert!(
            no_session,
            "a request without a sessionId must be an error reply"
        );
    }

    /// A session's MCP toggles live in its slot: listed with their enabled flag, changed per
    /// session, and unknown names or sessions are refused (SPEC §11).
    #[test]
    fn mcp_toggles_are_per_session_and_refuse_unknown_names() {
        use praxis_core::config::McpSource;
        use praxis_core::mcp::{Health, ServerStatus};
        let dir = temp_dir("mcp-toggles");
        let server = Server {
            new_agent: Box::new(|| unreachable!("no new agent is needed")),
            models: FakeModels,
            sessions_dir: dir.join("sessions"),
            commands: Vec::new(),
            trust: None,
            sessions: Mutex::new(HashMap::new()),
            client_reads: AtomicBool::new(false),
            client_diagnostics: AtomicBool::new(false),
        };
        let status = ServerStatus {
            name: "git".into(),
            source: McpSource::Plugin,
            status: Health::Connected,
            tools: vec!["git_status".into()],
            command: None,
        };
        let slot = Slot {
            agent: None,
            cancel: CancellationToken::new(),
            subagents: SubagentStops::default(),
            model: "m".into(),
            mode: Mode::Ask,
            mcp: vec![status],
            mcp_off: BTreeSet::new(),
        };
        server.sessions().insert("s".into(), slot);
        server.set_mcp_enabled("s", "git", false).unwrap();
        let listed = server.mcp_servers("s").unwrap();
        assert_eq!(
            listed,
            json!({"servers": [{"name": "git", "source": "plugin", "status": "connected",
                "tools": ["git_status"], "enabled": false}]})
        );
        assert!(server.set_mcp_enabled("s", "nope", true).is_err());
        assert!(server.set_mcp_enabled("gone", "git", true).is_err());
        assert!(server.mcp_servers("gone").is_err());
        server.set_mcp_enabled("s", "git", true).unwrap();
        assert_eq!(
            server.mcp_servers("s").unwrap()["servers"][0]["enabled"],
            true
        );
    }

    /// Stands in for a turn whose task panicked, so its agent never comes back.
    fn crashed_turn() -> Agent {
        panic!("turn crashed")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn closing_a_session_refuses_its_prompts_and_keeps_the_others() {
        let dir = temp_dir("close");
        let server = fake_server().await;
        let run = Client.v1().connect_with(praxis(&server, &dir), async |cx| {
            let init = v1::InitializeRequest::new(ProtocolVersion::V1);
            cx.send_request(init).block_task().await?;
            let first = cx
                .send_request(v1::NewSessionRequest::new(&dir))
                .block_task()
                .await?;
            let second = cx
                .send_request(v1::NewSessionRequest::new(&dir))
                .block_task()
                .await?;
            let close = v1::CloseSessionRequest::new(first.session_id.clone());
            cx.send_request(close).block_task().await?;
            let prompt = v1::PromptRequest::new(first.session_id, vec!["go".into()]);
            let refused = cx.send_request(prompt).block_task().await;
            assert!(refused.is_err(), "a closed session must refuse prompts");
            let prompt = v1::PromptRequest::new(second.session_id, vec!["go".into()]);
            let answered = cx.send_request(prompt).block_task().await;
            assert!(answered.is_ok(), "the other session must still answer");
            Ok(())
        });
        tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_crashed_turn_does_not_leave_the_next_prompt_refused_as_running() {
        let dir = temp_dir("crashed-turn");
        let server = Server {
            new_agent: Box::new(|| unreachable!("no new agent is needed")),
            models: FakeModels,
            sessions_dir: dir.join("sessions"),
            commands: Vec::new(),
            trust: None,
            sessions: Mutex::new(HashMap::new()),
            client_reads: AtomicBool::new(false),
            client_diagnostics: AtomicBool::new(false),
        };
        let url = "http://127.0.0.1:1".to_owned();
        let tools = Arc::new(ToolContext::new(&dir).unwrap());
        let provider = Arc::new(Provider::new(&url, None).unwrap());
        let config = AgentConfig {
            model: "m".into(),
            context_size: 100_000,
            system_prompt: "sys".into(),
            memory: Vec::new(),
            scoped_rules: Vec::new(),
            mask_keep_turns: 10,
            max_turn_requests: 10,
            profile: praxis_core::agent::profile::ModelProfile::default(),
            seed: None,
            policy: praxis_core::permissions::Policy::default(),
            masking: true,
        };
        let agent = Agent::new(provider, tools, config);
        let slot = Slot {
            subagents: agent.subagent_stops(),
            agent: Some(agent),
            cancel: CancellationToken::new(),
            model: "m".into(),
            mode: Mode::Ask,
            mcp: Vec::new(),
            mcp_off: BTreeSet::new(),
        };
        server.sessions().insert("s".into(), slot);
        let (_agent, _cancel) = server.begin_turn("s").unwrap();
        let joined = tokio::spawn(async { crashed_turn() }).await;
        let settled = server.settle_turn("s", joined);
        assert!(settled.is_err(), "a crashed turn must be reported");
        let next = server.begin_turn("s");
        let Err(err) = next else {
            panic!("the crashed session must not hand out an agent");
        };
        assert!(
            !format!("{err:?}").contains("already running"),
            "the next prompt must not be refused as running: {err:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_turn_that_panics_gets_an_error_and_the_next_prompt_is_not_refused_as_running() {
        let dir = temp_dir("panic-through-prompt");
        let server = fake_server().await;
        let run = Client.v1().connect_with(praxis(&server, &dir), async |cx| {
            let init = v1::InitializeRequest::new(ProtocolVersion::V1);
            cx.send_request(init).block_task().await?;
            let session = cx
                .send_request(v1::NewSessionRequest::new(&dir))
                .block_task()
                .await?;
            let crash =
                v1::PromptRequest::new(session.session_id.clone(), vec![CRASH_PROMPT.into()]);
            let crashed = cx.send_request(crash).block_task().await;
            assert!(
                crashed.is_err(),
                "a panicked turn must answer with an error"
            );
            let next = v1::PromptRequest::new(session.session_id, vec!["go".into()]);
            let refused = cx.send_request(next).block_task().await;
            let Err(err) = refused else {
                panic!("the panicked session must not run another turn");
            };
            assert!(
                !format!("{err:?}").contains("already running"),
                "the next prompt must not be refused as running: {err:?}"
            );
            Ok(())
        });
        tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .unwrap()
            .unwrap();
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
            .connect_with(praxis(&server, &dir), async |cx| {
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
    async fn a_new_session_announces_its_user_invocable_commands_with_their_source() {
        let dir = temp_dir("commands");
        let server = fake_server().await;
        let skill = |name: &str, user_invocable, dir: &str| praxis_core::plugin::Skill {
            name: name.into(),
            description: format!("{name} does things"),
            body: String::new(),
            model_invocable: true,
            user_invocable,
            dir: PathBuf::from(dir),
        };
        let commands = vec![
            skill("ponytail:lite", true, "/plugins/ponytail/skills/lite"),
            skill("ponytail:quiet", false, "/plugins/ponytail/skills/quiet"),
            skill("deploy", true, "/home/u/.claude/skills/deploy"),
        ];
        let log = Log::default();
        let on_update = log.clone();
        let run = Client
            .v1()
            .on_receive_notification(
                async move |note: v1::SessionNotification, _cx| {
                    push(&on_update, to_value(&note.update).unwrap().to_string());
                    Ok(())
                },
                on_receive_notification!(),
            )
            .connect_with(praxis_with(&server, &dir, commands), async |cx| {
                let init = v1::InitializeRequest::new(ProtocolVersion::V1);
                cx.send_request(init).block_task().await?;
                let session = v1::NewSessionRequest::new(&dir);
                cx.send_request(session).block_task().await?;
                Ok(())
            });
        tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .unwrap()
            .unwrap();
        let seen = log.lock().unwrap().clone();
        let update: Value = serde_json::from_str(&seen[0]).unwrap();
        assert_eq!(update["sessionUpdate"], "available_commands_update");
        let list = &update["availableCommands"];
        assert_eq!(list.as_array().unwrap().len(), 2, "{update}");
        assert_eq!(list[0]["name"], "ponytail:lite");
        assert_eq!(list[0]["description"], "ponytail:lite does things");
        assert_eq!(list[0]["_meta"]["source"], "ponytail");
        assert_eq!(list[1]["name"], "deploy");
        assert_eq!(list[1]["_meta"]["source"], "/home/u/.claude/skills");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_permission_mode_is_offered_and_switches_for_the_session() {
        let dir = temp_dir("mode-switch");
        let server = fake_server().await;
        let (opened, switched) = Client
            .v1()
            .connect_with(praxis(&server, &dir), async |cx| {
                let init = v1::InitializeRequest::new(ProtocolVersion::V1);
                cx.send_request(init).block_task().await?;
                let session = v1::NewSessionRequest::new(&dir);
                let session = cx.send_request(session).block_task().await?;
                let set = v1::SetSessionConfigOptionRequest::new(
                    session.session_id.clone(),
                    "mode",
                    "read-only",
                );
                let reply = cx.send_request(set).block_task().await?;
                Ok((
                    to_value(&session.config_options).unwrap(),
                    to_value(&reply.config_options).unwrap(),
                ))
            })
            .await
            .unwrap();
        assert_eq!(opened[1]["id"], "mode");
        assert_eq!(opened[1]["currentValue"], "ask");
        assert_eq!(opened[1]["options"][0]["value"], "read-only");
        assert_eq!(switched[1]["currentValue"], "read-only");
    }

    /// Switches the session through `modes` in order, then runs one prompt in which the fake model
    /// calls `name` with `input`. Returns how many permission requests the client got, and the
    /// requests the model received.
    async fn prompt_after_modes(
        dir: &Path,
        modes: &[&'static str],
        name: &str,
        input: Value,
    ) -> (usize, Vec<Value>) {
        let server = fake_server_calling(name, input).await;
        let asks = Arc::new(Mutex::new(0));
        let on_ask = asks.clone();
        let run = Client
            .v1()
            .on_receive_request(
                async move |_req: v1::RequestPermissionRequest, responder, _cx| {
                    *on_ask.lock().unwrap() += 1;
                    let chosen = v1::SelectedPermissionOutcome::new("reject_once");
                    let outcome = v1::RequestPermissionOutcome::Selected(chosen);
                    responder.respond(v1::RequestPermissionResponse::new(outcome))
                },
                on_receive_request!(),
            )
            .connect_with(praxis(&server, dir), async |cx| {
                let init = v1::InitializeRequest::new(ProtocolVersion::V1);
                cx.send_request(init).block_task().await?;
                let session = v1::NewSessionRequest::new(dir);
                let session = cx.send_request(session).block_task().await?;
                for mode in modes {
                    let set = v1::SetSessionConfigOptionRequest::new(
                        session.session_id.clone(),
                        "mode",
                        *mode,
                    );
                    cx.send_request(set).block_task().await?;
                }
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
        let asked = *asks.lock().unwrap();
        (asked, bodies)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_switch_to_read_only_denies_a_write_on_the_next_turn() {
        let dir = temp_dir("mode-read-only");
        let input = json!({"file_path": "ok.txt", "content": "x"});
        let (asked, requests) = prompt_after_modes(&dir, &["read-only"], "Write", input).await;
        assert_eq!(asked, 0, "read-only must deny without asking the client");
        assert!(!dir.join("ok.txt").exists(), "denied write must not run");
        // The second model request carries the tool result, which names the denial.
        let told = requests[1]["messages"].to_string();
        assert!(told.contains("can't run in read-only mode"), "{told}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_switch_to_auto_edit_applies_an_edit_without_asking() {
        let dir = temp_dir("mode-auto-edit");
        let input = json!({"file_path": "ok.txt", "content": "x"});
        let (asked, _) = prompt_after_modes(&dir, &["auto-edit"], "Write", input).await;
        assert_eq!(asked, 0, "auto-edit must apply the write without asking");
        assert_eq!(std::fs::read_to_string(dir.join("ok.txt")).unwrap(), "x");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn switching_back_to_ask_makes_the_next_edit_ask_again() {
        let dir = temp_dir("mode-back-to-ask");
        let input = json!({"file_path": "ok.txt", "content": "x"});
        let (asked, _) = prompt_after_modes(&dir, &["auto-edit", "ask"], "Write", input).await;
        assert_eq!(asked, 1, "ask must request permission for the write");
        assert!(!dir.join("ok.txt").exists(), "rejected write must not run");
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
            .connect_with(praxis(&server, &dir), async |cx| {
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
            .connect_with(praxis(&server, &dir), async |cx| {
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
            .connect_with(praxis(&server, dir), async |cx| {
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
            .connect_with(praxis(&server, &dir), async |cx| {
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
                // The rejected write replays as a call, with no text in this closure.
                r#""tool_call" "#,
                r#""tool_call_update" "#,
                r#""agent_message_chunk" done"#,
                "loaded"
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn loading_a_session_replays_its_tool_calls_in_order() {
        let dir = temp_dir("load-tools");
        let mut log = SessionLog::create(&dir.join("sessions"), &dir).unwrap();
        let id = log.id().to_owned();
        let call = ToolCall {
            id: "c1".into(),
            name: "Read".into(),
            arguments: r#"{"file_path":"a.rs"}"#.into(),
        };
        for message in [
            Message::User {
                content: "go".into(),
            },
            Message::Assistant {
                content: Some("Reading.".into()),
                tool_calls: vec![call],
            },
            Message::Tool {
                tool_call_id: "c1".into(),
                content: "fn a".into(),
            },
            Message::Assistant {
                content: Some("done".into()),
                tool_calls: vec![],
            },
        ] {
            log.append(&message).unwrap();
        }
        // Loading refuses a session that this fixture still holds open.
        drop(log);
        let server = fake_server().await;
        let log = Log::default();
        let on_update = log.clone();
        let run = Client
            .v1()
            .on_receive_notification(
                async move |note: v1::SessionNotification, _cx| {
                    let update = to_value(&note.update).unwrap();
                    let text = update["content"]["text"]
                        .as_str()
                        .or(update["title"].as_str())
                        .or(update["content"][0]["content"]["text"].as_str())
                        .unwrap_or_default();
                    push(&on_update, format!("{} {text}", update["sessionUpdate"]));
                    Ok(())
                },
                on_receive_notification!(),
            )
            .connect_with(praxis(&server, &dir), async |cx| {
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
                r#""agent_message_chunk" Reading."#,
                r#""tool_call" Read a.rs"#,
                r#""tool_call_update" fn a"#,
                r#""agent_message_chunk" done"#,
                "loaded"
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn loading_a_session_nests_a_subagents_calls_under_its_agent_call() {
        let dir = temp_dir("load-subagent");
        let mut log = SessionLog::create(&dir.join("sessions"), &dir).unwrap();
        let id = log.id().to_owned();
        let call = |id: &str, name: &str| ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: r#"{"file_path":"sub.txt","subagent_type":"general"}"#.into(),
        };
        log.append(&Message::Assistant {
            content: None,
            tool_calls: vec![call("c0", "Agent")],
        })
        .unwrap();
        log.append_subagent(
            "c0",
            &Message::Assistant {
                content: Some("inner text".into()),
                tool_calls: vec![call("c1", "Write")],
            },
        )
        .unwrap();
        log.append_subagent(
            "c0",
            &Message::Tool {
                tool_call_id: "c1".into(),
                content: "written".into(),
            },
        )
        .unwrap();
        log.append(&Message::Tool {
            tool_call_id: "c0".into(),
            content: "found it".into(),
        })
        .unwrap();
        drop(log);

        let server = fake_server().await;
        let updates = Log::default();
        let on_update = updates.clone();
        let run = Client
            .v1()
            .on_receive_notification(
                async move |note: v1::SessionNotification, _cx| {
                    push(&on_update, to_value(&note.update).unwrap().to_string());
                    Ok(())
                },
                on_receive_notification!(),
            )
            .connect_with(praxis(&server, &dir), async |cx| {
                let init = v1::InitializeRequest::new(ProtocolVersion::V1);
                cx.send_request(init).block_task().await?;
                cx.send_request(v1::LoadSessionRequest::new(id.clone(), &dir))
                    .block_task()
                    .await?;
                Ok(())
            });
        tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .unwrap()
            .unwrap();
        let updates: Vec<Value> = updates
            .lock()
            .unwrap()
            .iter()
            .map(|u| serde_json::from_str(u).unwrap())
            .collect();
        let nested = |id: &str| {
            updates
                .iter()
                .find(|u| u["toolCallId"] == id)
                .map(|u| u["_meta"]["praxis"]["parentToolCallId"].clone())
        };
        assert_eq!(nested("c0"), Some(Value::Null), "{updates:?}");
        assert_eq!(nested("c1"), Some(json!("c0")), "{updates:?}");
        assert!(
            !updates.iter().any(|u| u["content"]["text"] == "inner text"),
            "{updates:?}"
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
        let run = Client.v1().connect_with(praxis(&server, &dir), async |cx| {
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
    async fn loading_an_open_session_again_on_the_same_connection_succeeds() {
        let dir = temp_dir("reload");
        let server = fake_server().await;
        let run = Client
            .v1()
            .on_receive_notification(
                async move |_note: v1::SessionNotification, _cx| Ok(()),
                on_receive_notification!(),
            )
            .connect_with(praxis(&server, &dir), async |cx| {
                let init = v1::InitializeRequest::new(ProtocolVersion::V1);
                cx.send_request(init).block_task().await?;
                let session = cx
                    .send_request(v1::NewSessionRequest::new(&dir))
                    .block_task()
                    .await?;
                let id = session.session_id.to_string();
                for _ in 0..2 {
                    cx.send_request(v1::LoadSessionRequest::new(id.clone(), &dir))
                        .block_task()
                        .await?;
                }
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
            .connect_with(praxis(&server, &dir), async |cx| {
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
            .connect_with(praxis(&server, dir), async |cx| {
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
    async fn edit_refuses_a_dirty_buffer_and_says_to_save_it() {
        let dir = temp_dir("edit-dirty");
        std::fs::write(dir.join("a.txt"), "on disk\n").unwrap();
        let server = fake_server_script(&[
            ("Read", json!({"file_path": "a.txt"})),
            (
                "Edit",
                json!({"file_path": "a.txt", "old_string": "on disk", "new_string": "edited"}),
            ),
        ])
        .await;
        let (on_read, on_ask, on_update) = (Log::default(), Log::default(), Log::default());
        let output = on_update.clone();
        let run = Client
            .v1()
            .on_receive_request(
                async move |req: v1::ReadTextFileRequest, responder, _cx| {
                    push(&on_read, req.path.display().to_string());
                    responder.respond(v1::ReadTextFileResponse::new("unsaved buffer"))
                },
                on_receive_request!(),
            )
            .on_receive_request(
                async move |req: v1::RequestPermissionRequest, responder, _cx| {
                    push(
                        &on_ask,
                        req.tool_call.fields.title.clone().unwrap_or_default(),
                    );
                    let chosen = v1::SelectedPermissionOutcome::new("allow_once");
                    let outcome = v1::RequestPermissionOutcome::Selected(chosen);
                    responder.respond(v1::RequestPermissionResponse::new(outcome))
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
            .connect_with(praxis(&server, &dir), async |cx| {
                let init = serde_json::from_value::<v1::InitializeRequest>(json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {"fs": {"readTextFile": true}}
                }))
                .unwrap();
                cx.send_request(init).block_task().await?;
                let session = cx
                    .send_request(v1::NewSessionRequest::new(&dir))
                    .block_task()
                    .await?;
                let prompt = v1::PromptRequest::new(session.session_id, vec!["go".into()]);
                cx.send_request(prompt).block_task().await?;
                Ok(())
            });
        tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .unwrap()
            .unwrap();
        let output = output.lock().unwrap().join("");
        assert!(output.contains("unsaved changes in the editor"), "{output}");
        assert!(!output.contains("Read it again"), "{output}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn write_refuses_a_dirty_buffer_and_says_to_save_it() {
        let dir = temp_dir("write-dirty");
        std::fs::write(dir.join("a.txt"), "on disk\n").unwrap();
        let server = fake_server_script(&[
            ("Read", json!({"file_path": "a.txt"})),
            (
                "Write",
                json!({"file_path": "a.txt", "content": "written\n"}),
            ),
        ])
        .await;
        let (on_read, on_ask, on_update) = (Log::default(), Log::default(), Log::default());
        let output = on_update.clone();
        let run = Client
            .v1()
            .on_receive_request(
                async move |req: v1::ReadTextFileRequest, responder, _cx| {
                    push(&on_read, req.path.display().to_string());
                    responder.respond(v1::ReadTextFileResponse::new("unsaved buffer"))
                },
                on_receive_request!(),
            )
            .on_receive_request(
                async move |req: v1::RequestPermissionRequest, responder, _cx| {
                    push(
                        &on_ask,
                        req.tool_call.fields.title.clone().unwrap_or_default(),
                    );
                    let chosen = v1::SelectedPermissionOutcome::new("allow_once");
                    let outcome = v1::RequestPermissionOutcome::Selected(chosen);
                    responder.respond(v1::RequestPermissionResponse::new(outcome))
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
            .connect_with(praxis(&server, &dir), async |cx| {
                let init = serde_json::from_value::<v1::InitializeRequest>(json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {"fs": {"readTextFile": true}}
                }))
                .unwrap();
                cx.send_request(init).block_task().await?;
                let session = cx
                    .send_request(v1::NewSessionRequest::new(&dir))
                    .block_task()
                    .await?;
                let prompt = v1::PromptRequest::new(session.session_id, vec!["go".into()]);
                cx.send_request(prompt).block_task().await?;
                Ok(())
            });
        tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .unwrap()
            .unwrap();
        let output = output.lock().unwrap().join("");
        assert!(output.contains("unsaved changes in the editor"), "{output}");
        assert!(!output.contains("Read it again"), "{output}");
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "on disk\n"
        );
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
        let run = Client.v1().connect_with(praxis(&server, &dir), async |cx| {
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

    /// Runs one `Write` of `ok.txt`. A client that advertised diagnostics answers
    /// `_praxis/diagnostics` with one error and one warning. Returns the model's second request
    /// (it carries the tool result) and the diagnostics requests the client saw.
    async fn write_with_diagnostics(dir: &Path, advertise: bool) -> (String, Vec<Value>) {
        let server = fake_server().await;
        let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
        let on_diagnostics = seen.clone();
        let caps = if advertise {
            json!({"_meta": {"praxis": {"diagnostics": true}}})
        } else {
            json!({})
        };
        let init = serde_json::from_value::<v1::InitializeRequest>(json!({
            "protocolVersion": 1,
            "clientCapabilities": caps
        }))
        .unwrap();
        let run = Client
            .v1()
            .on_receive_request(
                async move |_req: v1::RequestPermissionRequest, responder, _cx| {
                    let chosen = v1::SelectedPermissionOutcome::new("allow_once");
                    let outcome = v1::RequestPermissionOutcome::Selected(chosen);
                    responder.respond(v1::RequestPermissionResponse::new(outcome))
                },
                on_receive_request!(),
            )
            .on_receive_request(
                async move |req: UntypedMessage, responder, _cx| {
                    on_diagnostics.lock().unwrap().push(json!({
                        "method": req.method(),
                        "params": req.params(),
                    }));
                    responder.respond(json!({"problems": [
                        {"line": 3, "severity": "error", "message": "expected `;`\nor `}`"},
                        {"line": 4, "severity": "warning", "message": "unused variable"},
                    ]}))
                },
                on_receive_request!(),
            )
            .connect_with(praxis(&server, dir), async |cx| {
                cx.send_request(init).block_task().await?;
                let session = cx
                    .send_request(v1::NewSessionRequest::new(dir))
                    .block_task()
                    .await?;
                let prompt = v1::PromptRequest::new(session.session_id, vec!["go".into()]);
                cx.send_request(prompt).block_task().await?;
                Ok(())
            });
        tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .unwrap()
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        let body = requests[1].body_json::<Value>().unwrap().to_string();
        let asked = seen.lock().unwrap().clone();
        (body, asked)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_client_that_reports_diagnostics_gets_an_edits_errors_in_its_result() {
        let dir = temp_dir("diagnostics-on");
        let (body, asked) = write_with_diagnostics(&dir, true).await;
        assert_eq!(asked.len(), 1, "{asked:?}");
        assert_eq!(asked[0]["method"], "_praxis/diagnostics");
        assert_eq!(
            asked[0]["params"]["path"],
            dir.join("ok.txt").display().to_string()
        );
        assert!(body.contains("Wrote"), "{body}");
        assert!(
            body.contains("Errors the editor reports for this file"),
            "{body}"
        );
        assert!(body.contains("line 3: expected `;` or `}`"), "{body}");
        assert!(!body.contains("unused variable"), "{body}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_client_without_diagnostics_gets_the_plain_edit_result() {
        let dir = temp_dir("diagnostics-off");
        let (body, asked) = write_with_diagnostics(&dir, false).await;
        assert!(asked.is_empty(), "{asked:?}");
        assert!(body.contains("Wrote"), "{body}");
        assert!(!body.contains("Errors the editor reports"), "{body}");
    }

    #[test]
    fn problem_lines_keep_only_errors_and_one_line_each() {
        let reply = json!({"problems": [
            {"line": 2, "severity": "error", "message": "a\r\nb"},
            {"line": 5, "severity": "warning", "message": "c"},
            {"severity": "error", "message": "no line"},
        ]});
        assert_eq!(problem_lines(&reply), vec!["line 2: a  b".to_owned()]);
        assert!(problem_lines(&json!({})).is_empty());
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
            before: None,
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
            first_token_ms: None,
            model_ms: 0,
        });
        assert_eq!(
            usage,
            json!({"sessionUpdate": "usage_update", "used": 5, "size": 9})
        );
        let notice = json(AgentEvent::Notice {
            severity: praxis_core::agent::Severity::Warning,
            title: "careful".into(),
            detail: None,
        });
        assert_eq!(notice["sessionUpdate"], "agent_message_chunk");
        assert!(update(AgentEvent::TurnEnded(StopReason::EndTurn)).is_none());
        let limit = stop_reason(StopReason::MaxTurnRequests);
        assert_eq!(limit, Some(v1::StopReason::MaxTurnRequests));
        assert_eq!(stop_reason(StopReason::ProviderError), None);
    }

    #[test]
    fn editor_reads_use_the_clients_spelling_of_a_symlinked_workspace() {
        let dir = temp_dir("symlinked-cwd");
        let real = dir.join("real");
        std::fs::create_dir_all(real.join("src")).unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let workspace = real.canonicalize().unwrap();
        assert_eq!(
            client_path(&workspace, &link, &workspace.join("src/a.rs")),
            link.join("src/a.rs")
        );
        assert_eq!(
            client_path(&workspace, &link, Path::new("/etc/passwd")),
            Path::new("/etc/passwd")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_revert_request_restores_the_file_and_refuses_a_path_outside_the_workspace() {
        let dir = temp_dir("revert-request");
        let server = fake_server_bodies(vec![
            tool_call_body(
                "c0",
                "Write",
                &json!({"file_path": "ok.txt", "content": "x"}),
            ),
            text_body("done"),
        ])
        .await;
        let caps = json!({"fs": {"readTextFile": false}});
        let init = serde_json::from_value::<v1::InitializeRequest>(json!({
            "protocolVersion": 1,
            "clientCapabilities": caps
        }))
        .unwrap();
        let run = Client
            .v1()
            .on_receive_request(
                async move |_req: v1::RequestPermissionRequest, responder, _cx| {
                    let chosen = v1::SelectedPermissionOutcome::new("allow_once");
                    let outcome = v1::RequestPermissionOutcome::Selected(chosen);
                    responder.respond(v1::RequestPermissionResponse::new(outcome))
                },
                on_receive_request!(),
            )
            .connect_with(praxis(&server, &dir), async |cx| {
                cx.send_request(init).block_task().await?;
                let session = cx
                    .send_request(v1::NewSessionRequest::new(&dir))
                    .block_task()
                    .await?
                    .session_id
                    .to_string();
                let prompt = v1::PromptRequest::new(session.clone(), vec!["make ok.txt".into()]);
                cx.send_request(prompt).block_task().await?;
                let revert = |path: &str| {
                    UntypedMessage::new(REVERT, json!({"sessionId": session, "path": path}))
                };
                let reverted = cx.send_request(revert("ok.txt")?).block_task().await?;
                let refused = cx.send_request(revert("../escape.txt")?).block_task().await;
                Ok((reverted, refused.is_err()))
            });
        let (reverted, refused) = tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reverted["path"], "ok.txt");
        assert!(
            refused,
            "a path outside the workspace must be an error reply"
        );
        assert!(
            !dir.join("ok.txt").exists(),
            "the revert must remove the file the turn created"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_rewind_request_restores_the_files_and_cuts_the_history_to_before_the_turn() {
        let dir = temp_dir("rewind-request");
        let server = fake_server_bodies(vec![
            tool_call_body(
                "c0",
                "Write",
                &json!({"file_path": "ok.txt", "content": "x"}),
            ),
            text_body("done"),
            text_body("again"),
        ])
        .await;
        let caps = json!({"fs": {"readTextFile": false}});
        let init = serde_json::from_value::<v1::InitializeRequest>(json!({
            "protocolVersion": 1,
            "clientCapabilities": caps
        }))
        .unwrap();
        let run = Client
            .v1()
            .on_receive_request(
                async move |_req: v1::RequestPermissionRequest, responder, _cx| {
                    let chosen = v1::SelectedPermissionOutcome::new("allow_once");
                    let outcome = v1::RequestPermissionOutcome::Selected(chosen);
                    responder.respond(v1::RequestPermissionResponse::new(outcome))
                },
                on_receive_request!(),
            )
            .connect_with(praxis(&server, &dir), async |cx| {
                cx.send_request(init).block_task().await?;
                let session = cx
                    .send_request(v1::NewSessionRequest::new(&dir))
                    .block_task()
                    .await?
                    .session_id
                    .to_string();
                let prompt = v1::PromptRequest::new(session.clone(), vec!["make ok.txt".into()]);
                cx.send_request(prompt).block_task().await?;
                let rewind = |turn: u64| {
                    UntypedMessage::new(REWIND, json!({"sessionId": session, "turn": turn}))
                };
                let rewound = cx.send_request(rewind(1)?).block_task().await?;
                let refused = cx.send_request(rewind(9)?).block_task().await;
                let again = v1::PromptRequest::new(session, vec!["again".into()]);
                cx.send_request(again).block_task().await?;
                Ok((rewound, refused.is_err()))
            });
        let (rewound, refused) = tokio::time::timeout(Duration::from_secs(20), run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rewound["turn"], 1);
        assert!(refused, "a turn outside the window must be an error reply");
        assert!(
            !dir.join("ok.txt").exists(),
            "the rewind must restore the file"
        );
        let requests = server.received_requests().await.unwrap();
        // requests[2] answers T13.7's note after the Write; the next turn is the last request.
        let body = requests
            .last()
            .unwrap()
            .body_json::<Value>()
            .unwrap()
            .to_string();
        assert!(body.contains("again"), "{body}");
        assert!(
            !body.contains("make ok.txt"),
            "the rewound turn must be gone: {body}"
        );
    }
}
