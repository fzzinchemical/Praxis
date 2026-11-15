//! Layered config (SPEC §8): defaults, user file, project file, `PRAXIS_*` variables, then
//! `--config-override` flags. Later layers win, permission rule lists append, and unknown keys
//! or wrong types are errors naming where they came from.

use crate::agent::DEFAULT_MAX_TURN_REQUESTS;
use crate::agent::profile::ProfileLayer;
use crate::claude_settings::{self, Permissions, Settings};
use crate::context::compaction::DEFAULT_MASK_KEEP_TURNS;
use crate::hooks::{self, Hook};
use crate::permissions::Mode;
use crate::trust::{TrustStore, hash};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use thiserror::Error;
use toml::{Table, Value};

const ENV_PREFIX: &str = "PRAXIS_";
/// `PRAXIS_*` variables that aren't config keys.
const ENV_RESERVED: [&str; 1] = ["PRAXIS_LOG_CONTENT"];
/// Lists that append across layers instead of replacing.
const MERGED_LISTS: [&str; 2] = ["permissions.allow", "permissions.deny"];
/// Never honoured from project config, even when approved (SPEC §8, §10).
const PROJECT_NEVER: [&str; 3] = ["lmstudio", "anthropic", "sandbox"];

/// The effective config. Every key has a default, so any layer may set any subset.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub default_model: Option<String>,
    pub lmstudio: LmStudio,
    pub anthropic: Anthropic,
    /// `[models.<alias>]`, names for `--model` and agents' `model:` (SPEC §5.1).
    pub models: BTreeMap<String, ModelAlias>,
    pub agent: AgentSettings,
    pub context: ContextSettings,
    pub permissions: PermissionRules,
    pub sandbox: SandboxSettings,
    // shortcut: patterns apply in name order, not file order; keep file order if overlapping
    // patterns ever need it.
    /// `[profiles."pattern"]`, the user layer of model profiles (SPEC §5.5).
    pub profiles: BTreeMap<String, ProfileLayer>,
    /// `[mcp.name]` servers (SPEC §13). Project ones run code, so they need approval.
    pub mcp: BTreeMap<String, McpServer>,
    /// Where each `mcp` server was declared. Filled by the loader, never read from a file.
    #[serde(skip)]
    pub mcp_sources: BTreeMap<String, McpSource>,
    /// Hooks from Claude Code settings files. Filled by the loader, never read from a file.
    #[serde(skip)]
    pub hooks: Vec<Hook>,
}

/// Which layer declared an MCP server, as the session's server list shows it (SPEC §11, §13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpSource {
    User,
    Project,
    Plugin,
    LmStudio,
}

impl McpSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Plugin => "plugin",
            Self::LmStudio => "lmstudio",
        }
    }
}

/// An MCP server, from `[mcp.name]` or Claude Code's `.mcp.json` shape (SPEC §8, §13).
#[derive(Debug, Clone)]
pub enum McpServer {
    Stdio(StdioServer),
    Http(HttpServer),
}

/// A server run as a child process.
#[derive(Clone)]
pub struct StdioServer {
    pub command: String,
    pub args: Vec<String>,
    /// Added to the server's environment. May hold secrets, so `Debug` shows names only.
    pub env: BTreeMap<String, String>,
}

impl std::fmt::Debug for StdioServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Args can carry an expanded plugin option too, so only their count is shown.
        f.debug_struct("StdioServer")
            .field("command", &self.command)
            .field("args", &self.args.len())
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// A streamable HTTP server. Header values may name variables as `${VAR}`; they resolve only
/// when connecting, so secrets never sit in config or logs.
#[derive(Clone)]
pub struct HttpServer {
    pub url: String,
    pub headers: BTreeMap<String, String>,
}

impl std::fmt::Debug for HttpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpServer")
            .field("url", &self.url)
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Both shapes in one table; `type` is optional, as in Claude Code.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMcpServer {
    #[serde(rename = "type")]
    kind: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

impl<'de> Deserialize<'de> for McpServer {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let raw = RawMcpServer::deserialize(d)?;
        let stdio_fields = !raw.args.is_empty() || !raw.env.is_empty();
        match (raw.kind.as_deref(), raw.command, raw.url) {
            (Some(kind), ..) if !matches!(kind, "stdio" | "http") => Err(D::Error::custom(
                format!("MCP server type `{kind}` isn't supported; use `stdio` or `http`"),
            )),
            (None | Some("stdio"), Some(command), None) if raw.headers.is_empty() => {
                Ok(Self::Stdio(StdioServer {
                    command,
                    args: raw.args,
                    env: raw.env,
                }))
            }
            (None | Some("http"), None, Some(url)) if !stdio_fields => Ok(Self::Http(HttpServer {
                url,
                headers: raw.headers,
            })),
            _ => Err(D::Error::custom(
                "an MCP server takes either `command`, `args` and `env`, or `url` and `headers`",
            )),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LmStudio {
    pub base_url: String,
    /// Names the variable holding the token, so the token itself never sits in a config file.
    pub api_key_env: String,
    /// Context to load a model with. Overrides the profile's.
    pub context: Option<u32>,
}

impl Default for LmStudio {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:1234".to_owned(),
            api_key_env: "LM_API_TOKEN".to_owned(),
            context: None,
        }
    }
}

/// `[anthropic]`, opt-in and never read from project config (SPEC §5.6, §10).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Anthropic {
    pub enabled: bool,
    /// Names the variable holding the key; the key itself is never in config.
    pub api_key_env: String,
    pub effort: String,
}

impl Default for Anthropic {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key_env: "ANTHROPIC_API_KEY".to_owned(),
            effort: "medium".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelAlias {
    /// Always `<backend>/<model>`; an alias can't name another alias.
    pub model: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    LmStudio,
    Anthropic,
}

/// A resolved model reference (SPEC §5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRef {
    pub backend: Backend,
    pub model: String,
}

fn with_backend(reference: &str) -> Option<ModelRef> {
    let (prefix, model) = reference.split_once('/')?;
    let backend = match prefix {
        "lmstudio" => Backend::LmStudio,
        "anthropic" => Backend::Anthropic,
        _ => return None,
    };
    Some(ModelRef {
        backend,
        model: model.to_owned(),
    })
}

impl Config {
    /// Resolves `<backend>/<model>`, an alias from `[models]`, or a bare LM Studio name.
    /// Any other prefix stays a bare name, since LM Studio keys such as
    /// `google/gemma-4-12b-qat` contain a publisher slash. An alias whose target lacks a
    /// known backend is an error naming its key.
    pub fn resolve_model(&self, reference: &str) -> Result<ModelRef, ConfigError> {
        if let Some(resolved) = with_backend(reference) {
            return Ok(resolved);
        }
        let Some(alias) = self.models.get(reference) else {
            return Ok(ModelRef {
                backend: Backend::LmStudio,
                model: reference.to_owned(),
            });
        };
        with_backend(&alias.model).ok_or_else(|| {
            error(
                &format!("models.{reference}.model"),
                format!(
                    "`{}` must be `lmstudio/<model>` or `anthropic/<model>`",
                    alias.model
                ),
            )
        })
    }

    /// [`Self::resolve_model`], but a reference to a backend the user hasn't enabled is an
    /// error, never a switch to another backend (SPEC §10, invariant 12).
    pub fn resolve_enabled_model(&self, reference: &str) -> Result<ModelRef, ConfigError> {
        let resolved = self.resolve_model(reference)?;
        if resolved.backend == Backend::Anthropic && !self.anthropic.enabled {
            return Err(error(
                "anthropic.enabled",
                format!(
                    "`{reference}` is an Anthropic model, but the backend is off; set \
                     `anthropic.enabled = true` in user config or a flag to use it"
                ),
            ));
        }
        Ok(resolved)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentSettings {
    pub max_turn_requests: u32,
    /// Load a model without asking (SPEC §5.2). Defaults on; a project file can't turn it on.
    pub auto_load: bool,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            max_turn_requests: DEFAULT_MAX_TURN_REQUESTS,
            auto_load: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextSettings {
    pub mask_keep_turns: u32,
    /// On since the T2.11 eval showed no drop in success (SPEC §7).
    pub filter_output: bool,
}

impl Default for ContextSettings {
    fn default() -> Self {
        Self {
            mask_keep_turns: DEFAULT_MASK_KEEP_TURNS,
            filter_output: true,
        }
    }
}

/// Rule syntax is checked by [`crate::permissions::Policy::new`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PermissionRules {
    pub mode: Mode,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode {
    #[default]
    On,
    Off,
}

/// `[sandbox]` (SPEC §9). Never read from project config.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxSettings {
    pub mode: SandboxMode,
    /// Allows TCP from commands.
    pub network: bool,
    pub extra_read: Vec<PathBuf>,
    pub extra_write: Vec<PathBuf>,
}

/// Where each layer comes from. Missing files are skipped.
#[derive(Debug, Default)]
pub struct Sources {
    pub user: Option<PathBuf>,
    pub project: Option<PathBuf>,
    /// The project's `.mcp.json`, read as the project's `mcp` table, with or without the
    /// `mcpServers` wrapper. Needs `project`, whose approval it shares.
    pub mcp_json: Option<PathBuf>,
    /// LM Studio's `~/.lmstudio/mcp.json`, same shape as `.mcp.json`. A user-level source that
    /// still needs approval, keyed by its own path; a name Praxis config defines wins (SPEC §8).
    pub lmstudio_mcp: Option<PathBuf>,
    /// The process environment; only `PRAXIS_*` names are read.
    pub env: Vec<(String, String)>,
    /// `--config-override` entries, as `KEY=VALUE` with dotted keys.
    pub overrides: Vec<String>,
    /// Where approvals of widening project config persist. `None` keeps none between runs.
    pub trust: Option<TrustStore>,
    /// `~/.claude/settings.json`, read below the praxis user layer (SPEC §12).
    pub claude_user: Option<PathBuf>,
    /// The project's `.claude/settings.json` then `settings.local.json`, below the project layer.
    pub claude_project: Vec<PathBuf>,
}

#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    /// Project items that were ignored, for the front end to show.
    pub notices: Vec<String>,
}

/// Names the file, variable or flag at fault, then the key and problem.
#[derive(Debug, Error)]
#[error("{origin}: {message}")]
pub struct ConfigError {
    origin: String,
    message: String,
}

fn error(origin: &str, message: impl ToString) -> ConfigError {
    ConfigError {
        origin: origin.to_owned(),
        message: message.to_string(),
    }
}

/// Loads without anyone to ask, so widening project config stays inactive unless it was
/// approved before.
pub fn load(sources: Sources) -> Result<Loaded, ConfigError> {
    load_with_approval(sources, &mut |_, _| false)
}

/// `approve(project config path, widening items as TOML)` asks the user. It is only called
/// when the items differ from what was approved before.
pub fn load_with_approval(
    sources: Sources,
    approve: &mut dyn FnMut(&Path, &str) -> bool,
) -> Result<Loaded, ConfigError> {
    let mut merged = Table::new();
    let mut notices = Vec::new();
    let mut hooks = Vec::new();
    // Names Praxis config defines, approved or not; LM Studio's servers can't take them.
    let mut defined = Vec::new();
    // Names of the project's approved MCP servers, so each can say it came from the project.
    let mut project_mcp = std::collections::BTreeSet::new();
    let mut lmstudio_mcp = std::collections::BTreeSet::new();
    // Claude Code's user file sits below the praxis user layer and needs no approval: it's the
    // user's own file (SPEC §8).
    if let Some(path) = &sources.claude_user {
        let settings = read_settings(path)?;
        merge(&mut merged, rules_table(&settings.permissions), "");
        if let Some(events) = &settings.hooks {
            parse_hooks(events, &mut hooks, &mut notices);
        }
    }
    if let Some(path) = &sources.user {
        merge(&mut merged, read_file(path)?, "");
    }
    if let Some(path) = &sources.project {
        let mut project = read_file(path)?;
        if let Some(json) = &sources.mcp_json {
            merge(&mut project, read_mcp_json(json)?, "");
        }
        defined.extend(mcp_names(&project));
        for key in PROJECT_NEVER {
            if remove(&mut project, key).is_some() {
                notices.push(format!(
                    "Ignored `{key}` from {}: project config can't set it.",
                    path.display()
                ));
            }
        }
        // Deny rules apply at once; allow rules and hooks widen, so they wait for approval.
        let mut project_hooks = Vec::new();
        for file in &sources.claude_project {
            let settings = read_settings(file)?;
            merge(&mut project, rules_table(&settings.permissions), "");
            project_hooks.extend(settings.hooks);
        }
        // Full access is never granted by a project, approved or not (SPEC §10).
        if mode_of(&project) == Some(Mode::FullAccess) {
            remove(&mut project, "permissions.mode");
            notices.push(format!(
                "Ignored `permissions.mode = \"full-access\"` from {}: only your own config can select it.",
                path.display()
            ));
        }
        let (widening, mut keys) =
            split_widening(&mut project, mode_of(&merged).unwrap_or_default());
        // Shown to the approver as TOML, but not merged into the config table: hooks are parsed.
        let mut shown = widening.clone();
        if !project_hooks.is_empty() {
            shown.insert(
                "hooks".to_owned(),
                Value::try_from(&project_hooks).map_err(|e| error("config", e))?,
            );
            keys.push("`hooks`".to_owned());
        }
        if !keys.is_empty() {
            let text = toml::to_string(&shown).map_err(|e| error("config", e))?;
            if is_approved(&sources, path, &text, approve)? {
                merge(&mut project, widening, "");
                for events in &project_hooks {
                    parse_hooks(events, &mut hooks, &mut notices);
                }
            } else {
                notices.push(format!(
                    "Ignored {} from {}: widening project config needs your approval.",
                    keys.join(", "),
                    path.display()
                ));
            }
        }
        if let Some(Value::Table(servers)) = project.get("mcp") {
            project_mcp.extend(servers.keys().cloned());
        }
        merge(&mut merged, project, "");
    }
    if let Some(path) = &sources.lmstudio_mcp {
        defined.extend(mcp_names(&merged));
        let mut lmstudio = read_mcp_json(path).unwrap_or_else(|e| {
            notices.push(format!("Ignored LM Studio's MCP servers: {e}."));
            Table::new()
        });
        if let Some(Value::Table(servers)) = lmstudio.get_mut("mcp") {
            servers.retain(|name, _| !defined.iter().any(|d| d == name));
        }
        let has_servers = lmstudio
            .get("mcp")
            .and_then(Value::as_table)
            .is_some_and(|servers| !servers.is_empty());
        if has_servers {
            let text = toml::to_string(&lmstudio).map_err(|e| error("config", e))?;
            if is_approved(&sources, path, &text, approve)? {
                if let Some(Value::Table(servers)) = lmstudio.get("mcp") {
                    lmstudio_mcp.extend(servers.keys().cloned());
                }
                merge(&mut merged, lmstudio, "");
            } else {
                notices.push(format!(
                    "Ignored `mcp` from {}: LM Studio's MCP servers need your approval.",
                    path.display()
                ));
            }
        }
    }
    let env = sources.env.iter().filter_map(|(name, value)| {
        let key = name.strip_prefix(ENV_PREFIX)?;
        let key = key.to_lowercase().replace("__", ".");
        (!ENV_RESERVED.contains(&name.as_str())).then(|| (name.clone(), key, value.as_str()))
    });
    for (origin, key, value) in env {
        merge(&mut merged, entry(&origin, &key, value)?, "");
    }
    for raw in &sources.overrides {
        let origin = format!("--config-override {raw}");
        let (key, value) = raw
            .split_once('=')
            .ok_or_else(|| error(&origin, "expected KEY=VALUE, such as agent.auto_load=true"))?;
        merge(&mut merged, entry(&origin, key, value)?, "");
    }
    let mut config: Config = merged
        .try_into()
        .map_err(|e: toml::de::Error| error("config", e.message()))?;
    config.mcp_sources = config
        .mcp
        .keys()
        .map(|name| {
            let source = if project_mcp.contains(name) {
                McpSource::Project
            } else if lmstudio_mcp.contains(name) {
                McpSource::LmStudio
            } else {
                McpSource::User
            };
            (name.clone(), source)
        })
        .collect();
    config.hooks = hooks;
    Ok(Loaded { config, notices })
}

/// Reads a Claude Code settings file; a missing one is empty.
fn read_settings(path: &Path) -> Result<Settings, ConfigError> {
    let settings =
        claude_settings::read(path).map_err(|e| error(&path.display().to_string(), e))?;
    Ok(settings.unwrap_or_default())
}

/// `permissions.allow`/`deny` as a `[permissions]` table, so they merge like the config's own.
fn rules_table(rules: &Permissions) -> Table {
    let list = |items: &[String]| Value::Array(items.iter().cloned().map(Value::String).collect());
    let mut table = Table::new();
    if !rules.allow.is_empty() {
        table.insert("allow".to_owned(), list(&rules.allow));
    }
    if !rules.deny.is_empty() {
        table.insert("deny".to_owned(), list(&rules.deny));
    }
    Table::from_iter((!table.is_empty()).then(|| ("permissions".to_owned(), Value::Table(table))))
}

/// Appends a settings file's hooks; skipped entries become notices.
fn parse_hooks(events: &serde_json::Value, out: &mut Vec<Hook>, notices: &mut Vec<String>) {
    let (found, warnings) = hooks::parse(events);
    out.extend(found);
    notices.extend(warnings);
}

fn read_file(path: &std::path::Path) -> Result<Table, ConfigError> {
    let origin = path.display().to_string();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Table::new()),
        Err(e) => return Err(error(&origin, e)),
    };
    // Parsed into `Config` once for the error, which shows the offending line and key.
    toml::from_str::<Config>(&text).map_err(|e| error(&origin, e))?;
    toml::from_str(&text).map_err(|e| error(&origin, e))
}

/// `.mcp.json` as a `mcp` table, so it goes through the same approval as `[mcp]`.
fn read_mcp_json(path: &Path) -> Result<Table, ConfigError> {
    let origin = path.display().to_string();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Table::new()),
        Err(e) => return Err(error(&origin, e)),
    };
    let mut value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| error(&origin, e))?;
    if let Some(wrapped) = value.get_mut("mcpServers") {
        value = wrapped.take();
    }
    // Parsed once for the error, which then names this file rather than `config`.
    serde_json::from_value::<BTreeMap<String, McpServer>>(value.clone())
        .map_err(|e| error(&origin, e))?;
    let servers = Value::try_from(value).map_err(|e| error(&origin, e))?;
    // An empty file must not ask for approval.
    let empty = servers.as_table().is_some_and(Table::is_empty);
    Ok(Table::from_iter(
        (!empty).then(|| ("mcp".to_owned(), servers)),
    ))
}

// shortcut: dotted keys can't reach a key that itself holds a dot, such as a profile pattern
// `qwen3.5*`; set those in a file.
/// One `key = value` from a variable or flag, validated on its own so the error names it.
fn entry(origin: &str, key: &str, raw: &str) -> Result<Table, ConfigError> {
    let table = nested(key, literal_or_string(raw));
    Config::deserialize(table.clone())
        .map_err(|e| error(origin, format!("`{key}`: {}", e.message())))?;
    Ok(table)
}

/// A TOML literal if `raw` parses as one, else plain text, so `default_model=qwen3.5-4b` works.
fn literal_or_string(raw: &str) -> Value {
    toml::from_str::<Table>(&format!("v = {raw}"))
        .ok()
        .and_then(|mut t| t.remove("v"))
        .unwrap_or_else(|| Value::String(raw.to_owned()))
}

/// Writes `field = value` into `[profiles."<key>"]` of the config file at `path`, editing the
/// text so comments survive. Writes nothing and errors if the value doesn't fit
/// `ProfileLayer`, or if the table is in a form this edit can't safely change.
pub fn set_profile_field(path: &Path, key: &str, field: &str, value: &str) -> Result<(), String> {
    let value = literal_or_string(value);
    let mut layer = Table::new();
    layer.insert(field.to_owned(), value.clone());
    ProfileLayer::deserialize(layer).map_err(|e| format!("`{field}`: {}", e.message()))?;

    let shown = path.display();
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{shown}: {e}")),
    };
    let header = format!("[profiles.{}]", Value::String(key.to_owned()));
    let line = format!("{field} = {value}");
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let head = lines.iter().position(|l| l.trim_end() == header);
    match head {
        Some(head) => {
            let next_table = lines[head + 1..]
                .iter()
                .position(|l| l.starts_with('['))
                .map_or(lines.len(), |i| head + 1 + i);
            let existing = lines[head + 1..next_table]
                .iter()
                .position(|l| l.split('=').next().map(str::trim) == Some(field));
            match existing {
                Some(i) => lines[head + 1 + i] = line,
                None => lines.insert(head + 1, line),
            }
        }
        None => {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push(header);
            lines.push(line);
        }
    }
    let mut new_text = lines.join("\n");
    new_text.push('\n');

    // Re-parsing catches what the line edit can't see, such as dotted keys defining the table.
    let parsed: Table = toml::from_str(&new_text).map_err(|e| format!("{shown}: {e}"))?;
    let stored = parsed
        .get("profiles")
        .and_then(|p| p.get(key))
        .and_then(|t| t.get(field));
    if stored != Some(&value) {
        return Err(format!(
            "can't set `{field}` in {shown} safely; edit the file by hand"
        ));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(path, new_text).map_err(|e| format!("{shown}: {e}"))
}

/// `a.b.c = value` as nested tables.
fn nested(key: &str, value: Value) -> Table {
    let mut table = Table::new();
    let mut parts = key.rsplit('.');
    table.insert(parts.next().unwrap_or(key).to_owned(), value);
    for part in parts {
        table = Table::from_iter([(part.to_owned(), Value::Table(table))]);
    }
    table
}

/// Moves the items that widen what runs out of `project`, and names them.
fn split_widening(project: &mut Table, user_mode: Mode) -> (Table, Vec<String>) {
    let mut keys = Vec::new();
    if mode_of(project) > Some(user_mode) {
        keys.push("permissions.mode");
    }
    if project
        .get("permissions")
        .and_then(|p| p.get("allow"))
        .is_some()
    {
        keys.push("permissions.allow");
    }
    let auto_load = project.get("agent").and_then(|a| a.get("auto_load"));
    if auto_load.and_then(Value::as_bool) == Some(true) {
        keys.push("agent.auto_load");
    }
    if project.contains_key("mcp") {
        keys.push("mcp");
    }
    let mut widening = Table::new();
    for key in &keys {
        if let Some(value) = remove(project, key) {
            merge(&mut widening, nested(key, value), "");
        }
    }
    (widening, keys.iter().map(|k| format!("`{k}`")).collect())
}

/// Whether widening config from `path` may apply: stored in the trust store, or approved now.
/// A new approval is stored, so the next run doesn't ask again.
fn is_approved(
    sources: &Sources,
    path: &Path,
    text: &str,
    approve: &mut dyn FnMut(&Path, &str) -> bool,
) -> Result<bool, ConfigError> {
    let digest = hash(text);
    let store_error = |e: String| error("trust store", e);
    let stored = match &sources.trust {
        Some(store) => store.is_approved(path, &digest).map_err(store_error)?,
        None => false,
    };
    let approved = stored || approve(path, text);
    if approved
        && !stored
        && let Some(store) = &sources.trust
    {
        store.approve(path, &digest).map_err(store_error)?;
    }
    Ok(approved)
}

/// The server names in a table's `mcp` key.
fn mcp_names(table: &Table) -> Vec<String> {
    table
        .get("mcp")
        .and_then(Value::as_table)
        .map(|servers| servers.keys().cloned().collect())
        .unwrap_or_default()
}

fn merge(base: &mut Table, over: Table, path: &str) {
    for (key, value) in over {
        let here = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        match (base.get_mut(&key), value) {
            (Some(Value::Table(base)), Value::Table(over)) => merge(base, over, &here),
            (Some(Value::Array(base)), Value::Array(over))
                if MERGED_LISTS.contains(&here.as_str()) =>
            {
                base.extend(over);
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

/// Removes a dotted key and returns its value.
fn remove(table: &mut Table, key: &str) -> Option<Value> {
    match key.split_once('.') {
        Some((head, rest)) => match table.get_mut(head) {
            Some(Value::Table(inner)) => remove(inner, rest),
            _ => None,
        },
        None => table.remove(key),
    }
}

/// The `permissions.mode` a validated layer sets, if any.
fn mode_of(table: &Table) -> Option<Mode> {
    let mode = table.get("permissions")?.get("mode")?.clone();
    Mode::deserialize(mode).ok()
}
