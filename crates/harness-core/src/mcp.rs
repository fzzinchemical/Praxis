//! MCP client over stdio (SPEC §13). Tools are listed once at connect and frozen, so the
//! `tools` array never changes mid-session (SPEC §7).

use crate::config::McpServer;
use crate::provider::ToolSchema;
use crate::tools::ToolOutput;
use futures::future::join_all;
use rmcp::model::{CallToolRequestParams, ContentBlock};
use rmcp::service::{NotificationContext, Peer, RunningService, ServiceError};
use rmcp::transport::TokioChildProcess;
use rmcp::{ClientHandler, RoleClient, ServiceExt as _};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::process::Stdio;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// How long one server may take to start and list its tools before the session goes on
/// without it.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

struct Tool {
    schema: ToolSchema,
    server: String,
    name: String,
}

/// Logs `list_changed` rather than applying it: the tools stay frozen (SPEC §13).
struct Handler {
    server: String,
}

impl ClientHandler for Handler {
    async fn on_tool_list_changed(&self, _: NotificationContext<RoleClient>) {
        tracing::info!(server = %self.server, "MCP tool list changed; keeping the session's list");
    }
}

type Client = RunningService<RoleClient, Handler>;

/// The connected servers and their frozen tools.
#[derive(Default)]
pub struct Mcp {
    clients: Mutex<HashMap<String, Client>>,
    servers: BTreeMap<String, McpServer>,
    /// Servers already restarted after a crash; a second crash leaves them down.
    reconnected: Mutex<HashSet<String>>,
    tools: Vec<Tool>,
}

impl std::fmt::Debug for Mcp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mcp")
            .field("servers", &self.servers.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

async fn connect_one(
    name: &str,
    server: &McpServer,
) -> Result<(Client, Vec<rmcp::model::Tool>), String> {
    let mut command = tokio::process::Command::new(&server.command);
    command.args(&server.args);
    // Keeps server noise off stdout, which carries ACP messages in `harness acp`.
    let (transport, _) = TokioChildProcess::builder(command)
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("couldn't start `{}`: {e}", server.command))?;
    let handler = Handler {
        server: name.to_owned(),
    };
    let client = handler.serve(transport).await.map_err(|e| e.to_string())?;
    let tools = client.list_all_tools().await.map_err(|e| e.to_string())?;
    Ok((client, tools))
}

impl Mcp {
    /// Connects to every server at once. A server that fails or takes longer than `timeout`
    /// is left out with a notice, so it never stops the session.
    // shortcut: waits up to `timeout` before the first prompt rather than connecting in the
    // background; move to a background join if real servers make startup feel slow.
    pub async fn connect(
        servers: &BTreeMap<String, McpServer>,
        timeout: Duration,
    ) -> (Self, Vec<String>) {
        let attempts = servers.iter().map(|(name, server)| async move {
            let result = tokio::time::timeout(timeout, connect_one(name, server))
                .await
                .unwrap_or_else(|_| Err(format!("no answer within {}s", timeout.as_secs())));
            (name, server, result)
        });
        let mut mcp = Self::default();
        let mut notices = Vec::new();
        for (server, config, result) in join_all(attempts).await {
            match result {
                Ok((client, tools)) => {
                    mcp.tools.extend(tools.into_iter().map(|tool| Tool {
                        schema: ToolSchema {
                            name: format!("mcp__{server}__{}", tool.name),
                            description: tool.description.unwrap_or_default().into_owned(),
                            parameters: Value::Object((*tool.input_schema).clone()),
                        },
                        server: server.clone(),
                        name: tool.name.into_owned(),
                    }));
                    mcp.clients
                        .get_mut()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(server.clone(), client);
                    mcp.servers.insert(server.clone(), config.clone());
                }
                Err(error) => notices.push(format!(
                    "MCP server `{server}` is unavailable this session: {error}"
                )),
            }
        }
        (mcp, notices)
    }

    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.iter().map(|tool| tool.schema.clone()).collect()
    }

    pub(crate) fn schema(&self, name: &str) -> Option<&ToolSchema> {
        self.find(name).map(|tool| &tool.schema)
    }

    fn find(&self, name: &str) -> Option<&Tool> {
        self.tools.iter().find(|tool| tool.schema.name == name)
    }

    fn peer(&self, server: &str) -> Option<Peer<RoleClient>> {
        let clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        clients.get(server).map(|client| client.peer().clone())
    }

    /// Restarts a crashed server once per session and says how that went. Its tools stay
    /// the ones listed at session start.
    async fn reconnect(&self, server: &str) -> &'static str {
        let first = self
            .reconnected
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(server.to_owned());
        let restarted = match self.servers.get(server).filter(|_| first) {
            Some(config) => tokio::time::timeout(CONNECT_TIMEOUT, connect_one(server, config))
                .await
                .ok()
                .and_then(Result::ok),
            None => None,
        };
        let mut clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        match restarted {
            Some((client, _)) => {
                clients.insert(server.to_owned(), client);
                " It was restarted, so the call can be tried again."
            }
            None => {
                clients.remove(server);
                " It stays down for the rest of the session."
            }
        }
    }

    /// Calls a tool by its `mcp__server__tool` name. Every failure comes back as an error
    /// result for the model.
    pub async fn call(&self, name: &str, input: Value, cancel: CancellationToken) -> ToolOutput {
        let Some(tool) = self.find(name) else {
            return ToolOutput::error(format!("Unknown tool `{name}`."));
        };
        let Some(client) = self.peer(&tool.server) else {
            return ToolOutput::error(format!("MCP server `{}` isn't connected.", tool.server));
        };
        let mut params = CallToolRequestParams::new(tool.name.clone());
        params.arguments = input.as_object().cloned();
        // shortcut: dropping the request on cancel doesn't send `notifications/cancelled`;
        // forward it when T4.3 handles failures.
        let result = tokio::select! {
            result = tokio::time::timeout(CALL_TIMEOUT, client.call_tool(params)) => result,
            () = cancel.cancelled() => return ToolOutput::error("Cancelled.".to_owned()),
        };
        let result = match result {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                let mut message = format!("MCP server `{}` failed: {error}.", tool.server);
                if matches!(
                    error,
                    ServiceError::TransportClosed | ServiceError::TransportSend(_)
                ) {
                    message.push_str(self.reconnect(&tool.server).await);
                }
                return ToolOutput::error(message);
            }
            Err(_) => {
                return ToolOutput::error(format!(
                    "`{name}` gave no answer within {}s.",
                    CALL_TIMEOUT.as_secs()
                ));
            }
        };
        let content = result
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::Text(text) => text.text.clone(),
                ContentBlock::Image(_) => "[image omitted]".to_owned(),
                ContentBlock::Audio(_) => "[audio omitted]".to_owned(),
                ContentBlock::Resource(_) | ContentBlock::ResourceLink(_) => {
                    "[resource omitted]".to_owned()
                }
                _ => "[content omitted]".to_owned(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        ToolOutput {
            content,
            is_error: result.is_error.unwrap_or(false),
            structured: result.structured_content.unwrap_or(Value::Null),
        }
    }
}
