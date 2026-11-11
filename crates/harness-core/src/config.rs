//! Layered config (SPEC §8): defaults, user file, project file, `HARNESS_*` variables, then
//! `--config-override` flags. Later layers win, permission rule lists append, and unknown keys
//! or wrong types are errors naming where they came from.

use crate::agent::DEFAULT_MAX_TURN_REQUESTS;
use crate::agent::profile::ProfileLayer;
use crate::context::compaction::DEFAULT_MASK_KEEP_TURNS;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use thiserror::Error;
use toml::{Table, Value};

const ENV_PREFIX: &str = "HARNESS_";
/// `HARNESS_*` variables that aren't config keys.
const ENV_RESERVED: [&str; 1] = ["HARNESS_LOG_CONTENT"];
/// Lists that append across layers instead of replacing.
const MERGED_LISTS: [&str; 2] = ["permissions.allow", "permissions.deny"];
// shortcut: until the trust gate (T3.4), project config drops everything that could widen
// what runs; T3.4 lets approved widening items through. `lmstudio` stays dropped for good.
const PROJECT_DROPPED: [&str; 3] = ["lmstudio", "agent.auto_load", "permissions.allow"];

/// The effective config. Every key has a default, so any layer may set any subset.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub default_model: Option<String>,
    pub lmstudio: LmStudio,
    pub agent: AgentSettings,
    pub context: ContextSettings,
    pub permissions: PermissionRules,
    // shortcut: patterns apply in name order, not file order; keep file order if overlapping
    // patterns ever need it.
    /// `[profiles."pattern"]`, the user layer of model profiles (SPEC §5.5).
    pub profiles: BTreeMap<String, ProfileLayer>,
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

/// Rules are parsed and merged here; enforcing them is T3.2.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PermissionRules {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
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

pub fn load(sources: Sources) -> Result<Loaded, ConfigError> {
    let mut merged = Table::new();
    let mut notices = Vec::new();
    if let Some(path) = &sources.user {
        merge(&mut merged, read_file(path)?, "");
    }
    if let Some(path) = &sources.project {
        let mut project = read_file(path)?;
        for key in PROJECT_DROPPED {
            if remove(&mut project, key) {
                notices.push(format!(
                    "Ignored `{key}` from {}: project config can't set it.",
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
    let mut table = Table::new();
    let mut parts = key.rsplit('.');
    table.insert(parts.next().unwrap_or(key).to_owned(), value);
    for part in parts {
        table = Table::from_iter([(part.to_owned(), Value::Table(table))]);
    }
    Config::deserialize(table.clone())
        .map_err(|e| error(origin, format!("`{key}`: {}", e.message())))?;
    Ok(table)
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

/// Removes a dotted key. Returns whether it was there.
fn remove(table: &mut Table, key: &str) -> bool {
    match key.split_once('.') {
        Some((head, rest)) => match table.get_mut(head) {
            Some(Value::Table(inner)) => remove(inner, rest),
            _ => false,
        },
        None => table.remove(key).is_some(),
    }
}
