//! Layered config (SPEC §8): defaults, user file, project file, `HARNESS_*` variables, then
//! `--config-override` flags. Later layers win, permission rule lists append, and unknown keys
//! or wrong types are errors naming where they came from.

use crate::agent::DEFAULT_MAX_TURN_REQUESTS;
use crate::agent::profile::ProfileLayer;
use crate::context::compaction::DEFAULT_MASK_KEEP_TURNS;
use crate::permissions::Mode;
use crate::trust::{TrustStore, hash};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use thiserror::Error;
use toml::{Table, Value};

const ENV_PREFIX: &str = "HARNESS_";
/// `HARNESS_*` variables that aren't config keys.
const ENV_RESERVED: [&str; 1] = ["HARNESS_LOG_CONTENT"];
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
}

/// A stdio MCP server.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Added to the server's environment. May hold secrets, so `Debug` shows names only.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl std::fmt::Debug for McpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Args can carry an expanded plugin option too, so only their count is shown.
        f.debug_struct("McpServer")
            .field("command", &self.command)
            .field("args", &self.args.len())
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .finish()
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
    /// Load a model without asking (SPEC §5.2).
    pub auto_load: bool,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            max_turn_requests: DEFAULT_MAX_TURN_REQUESTS,
            auto_load: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextSettings {
    pub mask_keep_turns: u32,
}

impl Default for ContextSettings {
    fn default() -> Self {
        Self {
            mask_keep_turns: DEFAULT_MASK_KEEP_TURNS,
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
    /// The process environment; only `HARNESS_*` names are read.
    pub env: Vec<(String, String)>,
    /// `--config-override` entries, as `KEY=VALUE` with dotted keys.
    pub overrides: Vec<String>,
    /// Where approvals of widening project config persist. `None` keeps none between runs.
    pub trust: Option<TrustStore>,
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
    if let Some(path) = &sources.user {
        merge(&mut merged, read_file(path)?, "");
    }
    if let Some(path) = &sources.project {
        let mut project = read_file(path)?;
        for key in PROJECT_NEVER {
            if remove(&mut project, key).is_some() {
                notices.push(format!(
                    "Ignored `{key}` from {}: project config can't set it.",
                    path.display()
                ));
            }
        }
        let (widening, keys) = split_widening(&mut project, mode_of(&merged).unwrap_or_default());
        if !keys.is_empty() {
            let text = toml::to_string(&widening).map_err(|e| error("config", e))?;
            let digest = hash(&text);
            let store_error = |e: String| error("trust store", e);
            let stored = match &sources.trust {
                Some(store) => store.is_approved(path, &digest).map_err(store_error)?,
                None => false,
            };
            let approved = stored || approve(path, &text);
            if approved
                && !stored
                && let Some(store) = &sources.trust
            {
                store.approve(path, &digest).map_err(store_error)?;
            }
            if approved {
                merge(&mut project, widening, "");
            } else {
                notices.push(format!(
                    "Ignored {} from {}: widening project config needs your approval.",
                    keys.join(", "),
                    path.display()
                ));
            }
        }
        merge(&mut merged, project, "");
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
    let config = merged
        .try_into()
        .map_err(|e: toml::de::Error| error("config", e.message()))?;
    Ok(Loaded { config, notices })
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

// shortcut: dotted keys can't reach a key that itself holds a dot, such as a profile pattern
// `qwen3.5*`; set those in a file.
/// One `key = value` from a variable or flag, validated on its own so the error names it.
fn entry(origin: &str, key: &str, raw: &str) -> Result<Table, ConfigError> {
    // A TOML literal if it parses as one, else plain text, so `default_model=qwen3.5-4b` works.
    let value = toml::from_str::<Table>(&format!("v = {raw}"))
        .ok()
        .and_then(|mut t| t.remove("v"))
        .unwrap_or_else(|| Value::String(raw.to_owned()));
    let table = nested(key, value);
    Config::deserialize(table.clone())
        .map_err(|e| error(origin, format!("`{key}`: {}", e.message())))?;
    Ok(table)
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
