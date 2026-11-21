//! The MCP Servers view's pure logic (SPEC §12, T4.7/T4.8). The answer to `_praxis/mcp_servers`
//! maps to rows here; the extension host only sends the requests and draws the rows.

use serde_json::{Value, json};

/// One MCP server as the agent reports it for this session.
#[derive(Debug, PartialEq)]
pub struct McpServer {
    pub name: String,
    /// `user`, `project`, `plugin` or `lmstudio`, shown as the agent names it.
    pub source: String,
    pub status: Status,
    pub tools: Vec<String>,
    pub enabled: bool,
}

/// The agent's `status`. A value this build does not know is kept, so a newer agent still lists its servers.
#[derive(Debug, PartialEq)]
pub enum Status {
    Connected,
    Failed,
    /// Listed from the config without a session, so nothing was connected to check.
    Configured,
    Unknown(String),
}

impl Status {
    fn parse(text: &str) -> Self {
        match text {
            "connected" => Status::Connected,
            "failed" => Status::Failed,
            "configured" => Status::Configured,
            other => Status::Unknown(other.to_owned()),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Status::Connected => "connected".to_owned(),
            Status::Failed => "failed".to_owned(),
            Status::Configured => "configured".to_owned(),
            Status::Unknown(text) => format!("unknown ({text})"),
        }
    }
}

impl McpServer {
    /// The row's secondary text: status, source and tool count.
    pub fn description(&self) -> String {
        let tools = self.tools.len();
        let plural = if tools == 1 { "" } else { "s" };
        format!(
            "{} · {} · {tools} tool{plural}",
            self.status.label(),
            self.source
        )
    }
}

/// Maps a `_praxis/mcp_servers` answer, `{"servers": [...]}`. One malformed row fails the whole
/// listing rather than hiding a server, as the other views do.
pub fn mcp_servers(result: &Value) -> Result<Vec<McpServer>, String> {
    let rows = result["servers"]
        .as_array()
        .ok_or("mcp_servers answer without a `servers` array")?;
    rows.iter().map(server).collect()
}

/// The message that draws the chat's MCP chips: every server with its status and enabled flag.
pub fn chips_message(servers: &[McpServer]) -> Value {
    let rows: Vec<Value> = servers
        .iter()
        .map(|server| {
            json!({
                "name": server.name,
                "status": server.status.label(),
                "enabled": server.enabled,
            })
        })
        .collect();
    json!({"type": "mcp_servers", "servers": rows})
}

fn server(row: &Value) -> Result<McpServer, String> {
    let text = |key: &str| {
        row[key]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("MCP server row without a string `{key}`"))
    };
    let tools = row["tools"]
        .as_array()
        .ok_or("MCP server row without a `tools` array")?
        .iter()
        .map(|tool| {
            tool.as_str()
                .or_else(|| tool["name"].as_str())
                .map(str::to_owned)
                .ok_or_else(|| "MCP tool without a name".to_owned())
        })
        .collect::<Result<_, _>>()?;
    Ok(McpServer {
        name: text("name")?,
        source: text("source")?,
        status: Status::parse(&text("status")?),
        tools,
        enabled: row["enabled"]
            .as_bool()
            .ok_or("MCP server row without an `enabled` flag")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_each_server_into_a_row() {
        let answer = json!({"servers": [
            {"name": "fs", "source": "project", "status": "connected",
             "tools": ["read", {"name": "write"}], "enabled": true},
            {"name": "git", "source": "lmstudio", "status": "failed", "tools": [], "enabled": false}
        ]});
        let rows = mcp_servers(&answer).expect("well-formed answer");
        assert_eq!(rows[0].name, "fs");
        assert_eq!(rows[0].tools, ["read", "write"]);
        assert_eq!(rows[0].description(), "connected · project · 2 tools");
        assert!(!rows[1].enabled);
        assert_eq!(rows[1].description(), "failed · lmstudio · 0 tools");
    }

    #[test]
    fn keeps_an_unknown_status_and_says_so() {
        let answer = json!({"servers": [
            {"name": "x", "source": "user", "status": "warming", "tools": ["a"], "enabled": true}
        ]});
        let rows = mcp_servers(&answer).expect("unknown status is not an error");
        assert_eq!(rows[0].status, Status::Unknown("warming".to_owned()));
        assert_eq!(rows[0].description(), "unknown (warming) · user · 1 tool");
    }

    #[test]
    fn one_malformed_row_fails_the_listing() {
        let answer = json!({"servers": [
            {"name": "ok", "source": "user", "status": "connected", "tools": [], "enabled": true},
            {"name": "bad", "source": "user", "status": "connected", "tools": []}
        ]});
        assert!(mcp_servers(&answer).is_err());
    }

    #[test]
    fn a_configured_server_from_the_offline_listing_claims_no_health() {
        let answer = json!({"servers": [
            {"name": "docs", "source": "user", "status": "configured", "tools": [], "enabled": true}
        ]});
        let rows = mcp_servers(&answer).expect("well-formed answer");
        assert_eq!(rows[0].status, Status::Configured);
        assert_eq!(rows[0].description(), "configured · user · 0 tools");
    }

    #[test]
    fn chips_message_carries_each_server_status_and_enabled_flag() {
        let answer = json!({"servers": [
            {"name": "fs", "source": "user", "status": "connected", "tools": [], "enabled": true},
            {"name": "git", "source": "user", "status": "failed", "tools": [], "enabled": false}
        ]});
        let servers = mcp_servers(&answer).expect("well-formed answer");
        assert_eq!(
            chips_message(&servers),
            json!({"type": "mcp_servers", "servers": [
                {"name": "fs", "status": "connected", "enabled": true},
                {"name": "git", "status": "failed", "enabled": false}
            ]})
        );
    }

    #[test]
    fn rejects_an_answer_without_servers() {
        assert!(mcp_servers(&json!({"tools": []})).is_err());
    }
}
