#![forbid(unsafe_code)]

//! The `praxis` binary: one-shot runs, the REPL, `praxis models` and `praxis acp` (SPEC §11).

mod acp;

use agent_client_protocol::ConnectTo;
use anyhow::Result;
use clap::{Parser, Subcommand};
use praxis_core::agent::profile::{ModelProfile, ProfileLayer};
use praxis_core::agent::{
    Agent, AgentConfig, AgentEvent, PermissionReply, StopReason, ToolOutcome, system_prompt,
};
use praxis_core::config::McpServer;
use praxis_core::config::{self, Config, SandboxMode, Sources};
use praxis_core::hooks::Hook;
use praxis_core::permissions::{Mode, Policy};
use praxis_core::plugin::{AgentDef, Skill, Store, run_command, standalone, standalone_agents};
use praxis_core::provider::{
    Capabilities, LoadPolicy, Message, ModelInfo, Provider, ProviderError, ReadyModel,
};
use praxis_core::sandbox::Sandbox;
use praxis_core::session::{self, SessionLog};
use praxis_core::summary::summarize;
use praxis_core::tools::ToolContext;
use praxis_core::trust::{TrustStore, hash};
use serde_json::Value;
use std::collections::BTreeMap;
use std::future::Future;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

// For models with no built-in context in their profile.
const DEFAULT_CONTEXT: u32 = 32768;
const EXIT_FAILURE: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_CANCELLED: u8 = 130;
/// The shell's "can't execute" code, so a refused sandbox reads like a command that didn't run.
const EXIT_SANDBOX: u8 = 126;

#[derive(Parser)]
#[command(name = "praxis", about = "Local-first coding agent for LM Studio")]
struct Cli {
    /// Run this prompt and exit. Without it, reads stdin if piped, otherwise starts a REPL.
    prompt: Option<String>,
    /// Model key. Defaults to the first model LM Studio has loaded.
    #[arg(long, global = true)]
    model: Option<String>,
    /// Set a config key for this run, as KEY=VALUE with a dotted key: `agent.auto_load=true`.
    #[arg(long = "config-override", value_name = "KEY=VALUE", global = true)]
    overrides: Vec<String>,
    /// Continue the session with this id, or without one the newest session in this workspace.
    // require_equals keeps `praxis --resume "fix bug"` a prompt; an ID needs `--resume=ID`.
    #[arg(long, value_name = "ID", num_args = 0..=1, require_equals = true)]
    resume: Option<Option<String>>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List models and their effective context.
    Models,
    /// Serve the Agent Client Protocol on stdio, for editors.
    Acp,
    /// Install, list, enable, disable or remove plugins (SPEC §12).
    #[command(subcommand)]
    Plugin(PluginCommand),
    /// Show or set a model's profile fields (SPEC §5.5).
    #[command(subcommand)]
    Profile(ProfileCommand),
    /// List the MCP servers this workspace configures. Connects to none, so no server runs.
    #[command(subcommand)]
    Mcp(McpCommand),
    /// List this workspace's saved sessions, newest first. Starts no model or MCP server.
    Sessions {
        /// Print a JSON array of `{id, title, started, last_active, folder}`.
        #[arg(long)]
        json: bool,
        /// Delete this session's log instead of listing. Refused while it is open elsewhere.
        #[arg(long, value_name = "ID", conflicts_with = "json")]
        delete: Option<String>,
        /// Move session ID into folder NAME instead of listing; an empty NAME removes it from its folder.
        #[arg(
            long,
            num_args = 2,
            value_names = ["ID", "NAME"],
            allow_hyphen_values = true,
            conflicts_with_all = ["json", "delete"]
        )]
        folder: Option<Vec<String>>,
        /// Create an empty folder NAME for this workspace. Its name is checked like a session's folder.
        #[arg(
            long,
            value_name = "NAME",
            allow_hyphen_values = true,
            conflicts_with_all = ["json", "delete", "folder", "folders"]
        )]
        new_folder: Option<String>,
        /// List the folders created for this workspace (see `--new-folder`); JSON with `--json`.
        #[arg(long, conflicts_with_all = ["delete", "folder"])]
        folders: bool,
    },
}

#[derive(Subcommand)]
enum McpCommand {
    List {
        /// Print `{"servers": [...]}` with each server's name, source, status and command when it needs approval.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum PluginCommand {
    /// Clone a git URL or local repo, pin its commit and show what it can run. Starts disabled.
    Install {
        source: String,
    },
    List {
        /// Print a JSON array with each plugin's state and what it can run.
        #[arg(long)]
        json: bool,
    },
    /// Show what the plugin can run, then enable it.
    Enable {
        name: String,
    },
    Disable {
        name: String,
    },
    /// Set one `userConfig` value. `sensitive` values are stored 0600 and never printed.
    Config {
        name: String,
        key: String,
        value: String,
    },
    /// Remove the plugin. Its data is kept unless removal is confirmed separately.
    Remove {
        name: String,
        /// Also delete `plugins/data/<name>/` without asking.
        #[arg(long)]
        delete_data: bool,
    },
}

#[derive(Subcommand)]
enum ProfileCommand {
    /// Print each effective field of the model's profile and the layer that set it.
    Show {
        /// Model key. Defaults to `--model`, then `default_model`.
        model: Option<String>,
        /// Print a JSON array of `{field, value, source}` rows, `value` null when unset.
        #[arg(long)]
        json: bool,
    },
    /// Write one field to the user config's `[profiles."<model>"]` table.
    Set { field: String, value: String },
}

/// The commands, skills and agents a details page lists, each with its description.
fn components(plugin: &praxis_core::plugin::Plugin) -> Vec<serde_json::Value> {
    let kinds = [
        ("command", plugin.commands()),
        ("skill", plugin.skills()),
        ("agent", plugin.agent_skills()),
    ];
    kinds
        .into_iter()
        .flat_map(|(kind, found)| {
            found.into_iter().map(move |skill| {
                serde_json::json!({"kind": kind, "name": skill.name, "description": skill.description})
            })
        })
        .collect()
}

/// Each `userConfig` key with its description and value. A `sensitive` value is never listed, only `set`.
fn settings(
    plugin: &praxis_core::plugin::Plugin,
    data_root: &std::path::Path,
) -> Result<Vec<serde_json::Value>, String> {
    let options = plugin.options(data_root)?;
    let Some(schema) = plugin.user_config.as_ref().and_then(|c| c.as_object()) else {
        return Ok(Vec::new());
    };
    Ok(schema
        .iter()
        .map(|(key, spec)| {
            let sensitive = spec["sensitive"].as_bool().unwrap_or(false);
            serde_json::json!({
                "key": key,
                "description": spec["description"].as_str().unwrap_or(""),
                "sensitive": sensitive,
                "set": options.contains_key(key),
                "value": options.get(key).filter(|_| !sensitive),
            })
        })
        .collect())
}

/// `praxis plugin ...`: no model or server needed.
fn plugin_command(command: PluginCommand) -> Result<u8> {
    let root = xdg_path("XDG_DATA_HOME", ".local/share", "plugins")
        .ok_or_else(|| anyhow::anyhow!("no XDG_DATA_HOME or HOME to install plugins under"))?;
    std::fs::create_dir_all(&root)?;
    let store = praxis_core::plugin::Store { root };
    let show = |plugin: &praxis_core::plugin::Plugin| -> Result<()> {
        println!("{} can run:", plugin.name);
        for line in plugin
            .runs(&store.data_root())
            .map_err(anyhow::Error::msg)?
        {
            println!("  {}", sanitize(&line));
        }
        Ok(())
    };
    match command {
        PluginCommand::Install { source } => {
            let (entry, plugin) = store.install(&source).map_err(anyhow::Error::msg)?;
            show(&plugin)?;
            println!("Installed {} at {}, disabled.", plugin.name, entry.commit);
            println!("Run `praxis plugin enable {}` to enable it.", plugin.name);
        }
        PluginCommand::List { json: true } => {
            let mut plugins = Vec::new();
            for (name, entry) in store.list().map_err(anyhow::Error::msg)? {
                // One broken plugin must not hide the others: the Remove action needs its row.
                let (readme, components, loaded) = match store.get(&name) {
                    Ok((_, plugin)) => (
                        plugin.readme(),
                        components(&plugin),
                        plugin
                            .runs(&store.data_root())
                            .and_then(|runs| Ok((runs, settings(&plugin, &store.data_root())?))),
                    ),
                    Err(error) => (None, Vec::new(), Err(error)),
                };
                let (can_run, settings, error) = match loaded {
                    Ok((runs, settings)) => (runs, settings, None),
                    Err(error) => (Vec::new(), Vec::new(), Some(error)),
                };
                plugins.push(serde_json::json!({
                    "name": name,
                    "source": entry.source,
                    "commit": entry.commit,
                    "enabled": entry.enabled,
                    "can_run": can_run,
                    "error": error,
                    "readme": readme,
                    "components": components,
                    "settings": settings,
                }));
            }
            println!("{}", serde_json::Value::Array(plugins));
        }
        PluginCommand::List { json: false } => {
            for (name, entry) in store.list().map_err(anyhow::Error::msg)? {
                let state = if entry.enabled { "enabled" } else { "disabled" };
                println!(
                    "{name} {} {state} {}",
                    entry.commit,
                    sanitize(&entry.source)
                );
            }
        }
        PluginCommand::Config { name, key, value } => {
            let (_, plugin) = store.get(&name).map_err(anyhow::Error::msg)?;
            plugin
                .set_option(&store.data_root(), &key, &value)
                .map_err(anyhow::Error::msg)?;
            println!("Set {key} for {name}.");
        }
        PluginCommand::Enable { name } => {
            show(&store.get(&name).map_err(anyhow::Error::msg)?.1)?;
            store.set_enabled(&name, true).map_err(anyhow::Error::msg)?;
            println!("Enabled {name}.");
        }
        PluginCommand::Disable { name } => {
            store
                .set_enabled(&name, false)
                .map_err(anyhow::Error::msg)?;
            println!("Disabled {name}.");
        }
        PluginCommand::Remove { name, delete_data } => {
            store.remove(&name).map_err(anyhow::Error::msg)?;
            println!("Removed {name}.");
            let data = store.data_root().join(&name);
            if !data.exists() {
                return Ok(0);
            }
            let confirmed = delete_data
                || std::io::stdin().is_terminal() && {
                    ask(&format!(
                        "Also delete its data at {}? [y/N]: ",
                        data.display()
                    ));
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line).is_ok()
                        && parse_reply(&line, false) == PermissionReply::AllowOnce
                };
            if confirmed {
                std::fs::remove_dir_all(&data)?;
                println!("Deleted {}.", data.display());
            } else {
                let data = data.display();
                println!("Kept its data at {data}. Pass --delete-data to remove it too.");
            }
        }
    }
    Ok(0)
}

/// `praxis sessions`: reads the session logs only, so the sidebar never starts a model or MCP server.
fn sessions_command(
    json: bool,
    delete: Option<String>,
    folder: Option<Vec<String>>,
    new_folder: Option<String>,
    folders: bool,
    workspace: &Path,
) -> Result<u8> {
    let Some(dir) = xdg_path("XDG_DATA_HOME", ".local/share", "sessions") else {
        eprintln!("Sessions need HOME or XDG_DATA_HOME to read from.");
        return Ok(EXIT_USAGE);
    };
    if let Some(id) = delete {
        session::delete(&dir, &id)?;
        return Ok(0);
    }
    if let Some(args) = folder {
        let [id, name] = args.as_slice() else {
            return Ok(EXIT_USAGE);
        };
        session::set_folder(&dir, id, name)?;
        return Ok(0);
    }
    if let Some(name) = new_folder {
        session::create_folder(&dir, workspace, &name)?;
        return Ok(0);
    }
    if folders {
        let names = session::created_folders(&dir, workspace)?;
        if json {
            println!("{}", serde_json::json!(names));
        } else {
            for name in &names {
                println!("{}", sanitize(name));
            }
        }
        return Ok(0);
    }
    let sessions = session::list(&dir, workspace)?;
    if json {
        let rows: Vec<Value> = sessions
            .iter()
            .map(|s| {
                serde_json::json!({
                    "id": s.id,
                    "title": s.title,
                    "started": s.started,
                    "last_active": s.last_active,
                    "folder": s.folder,
                })
            })
            .collect();
        println!("{}", Value::Array(rows));
    } else {
        for s in &sessions {
            println!("{} {}", s.id, sanitize(&s.title));
        }
    }
    Ok(0)
}

/// What enabled plugins add to a session (SPEC §12). Enabling is the approval to run them.
#[derive(Default)]
struct Plugins {
    skills: Vec<Skill>,
    commands: Vec<Skill>,
    agents: Vec<AgentDef>,
    hooks: Vec<(Hook, PathBuf)>,
    mcp: BTreeMap<String, McpServer>,
}

/// Lists the servers a session would start, with their source (SPEC §13). Nothing connects, so
/// the status is `configured` rather than a health check.
fn mcp_list(json: bool, config: &Config, workspace: &Path) -> Result<u8> {
    let plugins = load_plugins(workspace, config);
    let mut sources = config.mcp_sources.clone();
    sources.extend(
        plugins
            .mcp
            .keys()
            .map(|name| (name.clone(), praxis_core::mcp::McpSource::Plugin)),
    );
    let mut servers = config.mcp.clone();
    servers.extend(plugins.mcp);
    let rows: Vec<Value> = servers
        .keys()
        .map(|name| {
            let source = sources.get(name).map_or("user", |s| s.as_str());
            serde_json::json!({"name": name, "source": source, "status": "configured", "tools": [], "enabled": true})
        })
        .collect();
    if json {
        println!("{}", serde_json::json!({"servers": rows}));
    } else {
        for row in &rows {
            println!(
                "{} ({})",
                row["name"].as_str().unwrap_or_default(),
                row["status"].as_str().unwrap_or_default()
            );
        }
    }
    Ok(0)
}

/// Loads enabled plugins; a broken one is skipped with a notice so the session still starts.
fn load_plugins(workspace: &Path, config: &Config) -> Plugins {
    let mut out = Plugins::default();
    let found = standalone(&markdown_layers(workspace));
    // A standalone skill also runs as `/<name>`, as Claude Code does.
    out.commands.extend(found.skills.iter().cloned());
    out.skills.extend(found.skills);
    out.commands.extend(found.commands);
    let (agents, errors) = standalone_agents(&agent_layers(workspace), config);
    out.agents.extend(agents);
    for e in errors {
        eprintln!("Agent skipped: {}", sanitize(&e));
    }
    // Claude Code settings hooks run from the project, so the workspace stands in for a plugin root.
    out.hooks.extend(
        config
            .hooks
            .iter()
            .map(|h| (h.clone(), workspace.to_path_buf())),
    );
    let Some(root) = xdg_path("XDG_DATA_HOME", ".local/share", "plugins") else {
        return out;
    };
    let store = Store { root };
    let list = match store.list() {
        Ok(list) => list,
        Err(e) => {
            eprintln!("Plugins skipped: {}", sanitize(&e));
            return out;
        }
    };
    for name in list.into_iter().filter(|(_, e)| e.enabled).map(|(n, _)| n) {
        let loaded = store.get(&name).and_then(|(_, plugin)| {
            let (hooks, warnings) = plugin.hooks()?;
            let mcp = plugin.mcp_servers(workspace, &store.data_root())?;
            Ok((plugin, hooks, warnings, mcp))
        });
        let (plugin, hooks, warnings, mcp) = match loaded {
            Ok(loaded) => loaded,
            Err(e) => {
                eprintln!("Plugin `{name}` skipped: {}", sanitize(&e));
                continue;
            }
        };
        for warning in warnings.iter().chain(&plugin.warnings) {
            eprintln!("Plugin `{name}`: {}", sanitize(warning));
        }
        // A plugin skill is also a `/<plugin>:<name>` command, as a standalone skill is.
        let skills = plugin.skills();
        out.commands.extend(skills.iter().cloned());
        out.skills.extend(skills);
        out.commands.extend(plugin.commands());
        out.agents.extend(plugin.agents(config));
        out.hooks
            .extend(hooks.into_iter().map(|h| (h, plugin.root.clone())));
        out.mcp.extend(mcp);
    }
    out
}

/// Roots of the standalone skill and command layers, highest precedence first (SPEC §12).
fn markdown_layers(workspace: &Path) -> Vec<PathBuf> {
    let mut layers = user_layers();
    layers.extend(project_layers(workspace));
    layers
}

/// Roots of the standalone agent layers. Agents rank project above user (SPEC §12), unlike
/// skills and commands.
fn agent_layers(workspace: &Path) -> Vec<PathBuf> {
    let mut layers = project_layers(workspace);
    layers.extend(user_layers());
    layers
}

/// `~/.config/praxis` then `~/.claude`, highest precedence first.
fn user_layers() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => home.as_ref().map(|h| h.join(".config")),
    };
    [
        config.map(|c| c.join("praxis")),
        home.map(|h| h.join(".claude")),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// `.praxis` then `.claude` in the workspace, highest precedence first.
fn project_layers(workspace: &Path) -> Vec<PathBuf> {
    vec![workspace.join(".praxis"), workspace.join(".claude")]
}

/// `$<xdg_var>/praxis/<file>`, else `~/<fallback>/praxis/<file>`.
fn xdg_path(xdg_var: &str, fallback: &str, file: &str) -> Option<PathBuf> {
    xdg_dir(xdg_var, fallback).map(|dir| dir.join(file))
}

/// `$<xdg_var>/praxis`, else `~/<fallback>/praxis`.
fn xdg_dir(xdg_var: &str, fallback: &str) -> Option<PathBuf> {
    let base = match std::env::var_os(xdg_var) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(fallback),
    };
    Some(base.join("praxis"))
}

/// Only a person at a terminal can approve; anything else leaves the items inactive (SPEC §8).
fn approve_project_config(path: &Path, items: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        return false;
    }
    ask(&format!(
        "\n{} wants to widen what runs here:\n{}\nApprove? Any later change asks again. [y/N]: ",
        path.display(),
        sanitize(items)
    ));
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).is_ok()
        && parse_reply(&line, false) == PermissionReply::AllowOnce
}

/// Asks at the terminal and takes only an explicit yes. Without a terminal nobody can answer.
fn terminal_approves(question: &str, terminal: Option<&mut Terminal>) -> bool {
    let Some(terminal) = terminal else {
        return false;
    };
    ask(question);
    // The terminal reader owns stdin, so wait on its channel from this sync callback.
    tokio::task::block_in_place(|| terminal.blocking_recv())
        .is_some_and(|line| parse_reply(&line, false) == PermissionReply::AllowOnce)
}

/// A repository's memory file is labelled as instructions only once its exact expanded text is
/// approved (SPEC §8). A changed file asks again; a store error counts as not approved.
fn approve_memory(
    store: Option<&TrustStore>,
    path: &Path,
    text: &str,
    terminal: Option<&mut Terminal>,
) -> bool {
    let digest = hash(text);
    match store.map(|store| store.is_approved(path, &digest)) {
        Some(Ok(true)) => return true,
        Some(Err(error)) => eprintln!("{}", sanitize(&error)),
        Some(Ok(false)) | None => {}
    }
    let asked = terminal_approves(
        &format!(
            "\n{} holds instructions for this repository.\nFollow them as instructions? Any later change asks again. [y/N]: ",
            sanitize(&path.display().to_string())
        ),
        terminal,
    );
    if asked
        && let Some(store) = store
        && let Err(error) = store.approve(path, &digest)
    {
        eprintln!("{}", sanitize(&error));
    }
    asked
}

fn load_config(workspace: &Path, overrides: Vec<String>) -> Result<(Config, Policy), String> {
    let sources = Sources {
        user: xdg_path("XDG_CONFIG_HOME", ".config", "config.toml"),
        project: Some(workspace.join(".praxis/config.toml")),
        mcp_json: Some(workspace.join(".mcp.json")),
        lmstudio_mcp: std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".lmstudio/mcp.json")),
        // Non-Unicode variables can't hold config values, so they're skipped.
        env: std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect(),
        overrides,
        trust: xdg_path("XDG_DATA_HOME", ".local/share", "trust.json").map(TrustStore::new),
        claude_user: std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".claude/settings.json")),
        claude_project: [".claude/settings.json", ".claude/settings.local.json"]
            .map(|f| workspace.join(f))
            .to_vec(),
    };
    let loaded = config::load_with_approval(sources, &mut approve_project_config)
        .map_err(|e| e.to_string())?;
    for notice in &loaded.notices {
        eprintln!("{notice}");
    }
    let rules = &loaded.config.permissions;
    let policy = Policy::new(rules.mode, &rules.allow, &rules.deny)
        .map_err(|e| format!("permissions: {e}"))?;
    Ok((loaded.config, policy))
}

fn tool_context(workspace: &Path, config: &Config) -> Result<ToolContext> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    // The user instruction files are already in the prompt, so reading them exposes nothing new;
    // the files their index lines name are what the user chose to show on demand.
    let user_files: Vec<PathBuf> = [
        home.as_ref().map(|home| home.join(".claude/CLAUDE.md")),
        xdg_dir("XDG_CONFIG_HOME", ".config").map(|dir| dir.join("AGENTS.md")),
    ]
    .into_iter()
    .flatten()
    .collect();
    let indexed: Vec<PathBuf> = match &home {
        Some(home) => user_files
            .iter()
            .filter_map(|file| Some((std::fs::read_to_string(file).ok()?, file.parent()?)))
            .flat_map(|(text, dir)| praxis_core::context::prefix::index_paths(&text, home, dir))
            .collect(),
        None => Vec::new(),
    };
    let tools = ToolContext::new(workspace)?
        .with_output_filter(config.context.filter_output)
        .with_readable(user_files.into_iter().chain(indexed));
    let settings = &config.sandbox;
    if settings.mode == SandboxMode::Off {
        static NOTICE: std::sync::Once = std::sync::Once::new();
        NOTICE.call_once(|| eprintln!("The sandbox is off: commands run with your full access."));
        return Ok(tools);
    }
    Ok(tools.with_sandbox(Sandbox {
        helper: std::env::current_exe()?,
        network: settings.network,
        extra_read: settings.extra_read.clone(),
        extra_write: settings.extra_write.clone(),
    }))
}

fn user_profiles(config: &Config) -> Vec<(String, ProfileLayer)> {
    config.profiles.clone().into_iter().collect()
}

fn vendor_preset(config: &Config, key: &str) -> Option<ProfileLayer> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    praxis_core::agent::profile::vendor_layer(&config.lmstudio.base_url, &home, key)
}

/// The model's profile with every layer: the vendor's preset, built-ins, `capabilities` and
/// the user's `[profiles]`.
fn model_profile(config: &Config, key: &str, capabilities: Option<&Capabilities>) -> ModelProfile {
    let vendor = vendor_preset(config, key);
    praxis_core::agent::profile::resolve(key, capabilities, vendor.as_ref(), &user_profiles(config))
}

/// `praxis profile show|set`. Starts no model; `show` asks LM Studio only for capabilities.
async fn profile_command(
    command: ProfileCommand,
    model: Option<String>,
    config: &Config,
    provider: &Provider,
) -> Result<u8> {
    match command {
        ProfileCommand::Show {
            model: positional,
            json,
        } => {
            let Some(reference) = positional
                .or(model)
                .or_else(|| config.default_model.clone())
            else {
                anyhow::bail!("pass a model or set default_model");
            };
            let key = config.resolve_model(&reference)?.model;
            let capabilities = match provider.list_models().await {
                Ok(models) => models
                    .into_iter()
                    .find(|m| m.key == key)
                    .map(|m| m.capabilities),
                Err(error) => {
                    eprintln!("LM Studio is unreachable, so its capabilities are unknown: {error}");
                    None
                }
            };
            let vendor = vendor_preset(config, &key);
            let rows = praxis_core::agent::profile::explain(
                &key,
                capabilities.as_ref(),
                vendor.as_ref(),
                &user_profiles(config),
            );
            if json {
                let array: Vec<Value> = rows
                    .into_iter()
                    .map(|(field, value, source)| {
                        serde_json::json!({"field": field, "value": value, "source": source})
                    })
                    .collect();
                println!("{}", Value::Array(array));
                return Ok(0);
            }
            for (field, value, source) in rows {
                println!("{field} = {}  ({source})", shown(&value));
            }
            Ok(0)
        }
        ProfileCommand::Set { field, value } => {
            let Some(reference) = model.or_else(|| config.default_model.clone()) else {
                anyhow::bail!("pass a model or set default_model");
            };
            let key = config.resolve_model(&reference)?.model;
            let Some(path) = xdg_path("XDG_CONFIG_HOME", ".config", "config.toml") else {
                eprintln!("Profile set needs HOME or XDG_CONFIG_HOME to find the user config.");
                return Ok(EXIT_USAGE);
            };
            if let Err(error) = config::set_profile_field(&path, &key, &field, &value) {
                eprintln!("{error}");
                return Ok(EXIT_USAGE);
            }
            println!(
                "Set {field} = {value} in [profiles.\"{key}\"] ({})",
                path.display()
            );
            Ok(0)
        }
    }
}

/// A profile value as `show` prints it: plain text for strings, `unset` for no value.
fn shown(value: &Value) -> String {
    match value {
        Value::Null => "unset".to_owned(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Lines typed at the terminal, shared by the REPL and approvals. Stdin only ever counts as a
/// person when it's a terminal: piped input could come from anywhere, so it can't approve.
type Terminal = mpsc::UnboundedReceiver<String>;

fn spawn_terminal_reader() -> Terminal {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// Removes control characters such as terminal escapes: model text is untrusted and could
/// otherwise repaint the screen, for example to fake a permission prompt (SPEC §10).
fn sanitize(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

fn ask(question: &str) {
    eprint!("{question}");
    // A prompt that fails to display still gets no answer, so nothing can run from this.
    let _ = std::io::stderr().flush();
}

fn parse_reply(answer: &str, can_always: bool) -> PermissionReply {
    match answer.trim().to_lowercase().as_str() {
        "y" | "yes" => PermissionReply::AllowOnce,
        "a" | "always" if can_always => PermissionReply::AllowAlways,
        "r" | "reject" => PermissionReply::RejectAlways,
        _ => PermissionReply::RejectOnce,
    }
}

async fn ask_permission(
    name: &str,
    input: &Value,
    can_always: bool,
    terminal: &mut Terminal,
    cancel: &CancellationToken,
) -> PermissionReply {
    let shown = serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string());
    let options = if can_always {
        "[y]es once, [a]lways this session, [n]o, [r]eject for this session"
    } else {
        "[y]es once, [n]o, [r]eject for this session"
    };
    ask(&format!(
        "\nAllow {name}?\n{}\n{options}: ",
        sanitize(&shown)
    ));
    tokio::select! {
        line = terminal.recv() => match line {
            Some(line) => parse_reply(&line, can_always),
            None => PermissionReply::RejectOnce,
        },
        () = cancel.cancelled() => PermissionReply::Cancelled,
    }
}

/// Holds the token of the running turn. Ctrl-C cancels it once; with nothing left to cancel,
/// the next one exits.
#[derive(Clone)]
struct Interrupt(Arc<Mutex<CancellationToken>>);

impl Interrupt {
    fn install() -> Self {
        let idle = CancellationToken::new();
        idle.cancel();
        let interrupt = Self(Arc::new(Mutex::new(idle)));
        let handler = interrupt.clone();
        tokio::spawn(async move {
            while tokio::signal::ctrl_c().await.is_ok() {
                let current = handler.current();
                if current.is_cancelled() {
                    std::process::exit(EXIT_CANCELLED.into());
                }
                current.cancel();
            }
        });
        interrupt
    }

    fn current(&self) -> CancellationToken {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn begin_turn(&self) -> CancellationToken {
        let token = CancellationToken::new();
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = token.clone();
        token
    }
}

async fn run_turn(
    mut agent: Agent,
    prompt: String,
    terminal: &mut Option<Terminal>,
    interrupt: &Interrupt,
) -> Result<(Agent, StopReason)> {
    let cancel = interrupt.begin_turn();
    let (tx, mut rx) = mpsc::channel(64);
    let turn_cancel = cancel.clone();
    let start = agent.history().len();
    let task = tokio::spawn(async move {
        agent.run_turn(prompt, &tx, turn_cancel).await;
        agent
    });
    let mut stdout = std::io::stdout();
    let mut stop = StopReason::ProviderError;
    let mut mid_line = false;
    while let Some(event) = rx.recv().await {
        let event = match event {
            // A subagent's text stays out of the terminal view; its tool calls and prompts show as before.
            AgentEvent::Subagent { event, .. } if matches!(*event, AgentEvent::TextDelta(_)) => {
                continue;
            }
            AgentEvent::Subagent { event, .. } => *event,
            other => other,
        };
        match event {
            AgentEvent::TextDelta(text) => {
                let text = sanitize(&text);
                // An empty chunk after sanitizing leaves the cursor where it was.
                if !text.is_empty() {
                    mid_line = !text.ends_with('\n');
                }
                stdout.write_all(text.as_bytes())?;
                stdout.flush()?;
            }
            // Reasoning stays out of the main view (SPEC §11). The terminal shows no diffs, so a
            // call's after content is dropped here too.
            AgentEvent::ThinkingDelta(_) | AgentEvent::ToolChanged { .. } => {}
            AgentEvent::ToolStarted { title, .. } => {
                let title = sanitize(&title).replace('\n', " ");
                eprintln!("\n> {title}");
            }
            AgentEvent::ToolFinished { outcome, .. } => match outcome {
                ToolOutcome::Completed(output) if output.is_error => {
                    let first = output.content.lines().next().unwrap_or_default();
                    eprintln!("  error: {}", sanitize(first));
                }
                ToolOutcome::Completed(_) => {}
                ToolOutcome::Cancelled => eprintln!("  cancelled"),
            },
            AgentEvent::PermissionRequest {
                name,
                input,
                can_always,
                respond,
                ..
            } => match terminal {
                Some(terminal) => {
                    let reply = ask_permission(&name, &input, can_always, terminal, &cancel).await;
                    // A send failure means the turn already ended, so nothing can run.
                    let _ = respond.send(reply);
                }
                None => {
                    eprintln!("  denied: {name} needs approval and nobody can answer here");
                    drop(respond);
                }
            },
            AgentEvent::Usage {
                used_tokens,
                context_size,
                ..
            } => eprintln!("[context {used_tokens}/{context_size}]"),
            AgentEvent::Compaction(state) => eprintln!("[compaction {state:?}]"),
            AgentEvent::Notice { title, .. } => eprintln!("{}", sanitize(&title)),
            AgentEvent::TurnEnded(reason) => stop = reason,
            // Subagent wrappers are unwrapped above; this arm only keeps the match exhaustive.
            AgentEvent::Subagent { .. } => {}
        }
    }
    if mid_line {
        writeln!(stdout)?;
    }
    let agent = task.await?;
    // shortcut: compaction can rewrite history mid-turn, so this may summarise the wrong messages; the summary only ever reads the tool log.
    let turn = agent.history().get(start..).unwrap_or_default();
    if turn.iter().any(|m| matches!(m, Message::Tool { .. })) {
        eprintln!("{}", summarize(turn).render());
    }
    cancel.cancel();
    Ok((agent, stop))
}

fn exit_code(stop: StopReason) -> u8 {
    match stop {
        StopReason::EndTurn => 0,
        StopReason::Cancelled => EXIT_CANCELLED,
        StopReason::MaxTokens => {
            eprintln!("The model hit its output limit, so the reply is cut short.");
            EXIT_FAILURE
        }
        StopReason::MaxTurnRequests => {
            eprintln!("Stopped at the request limit for one turn (`agent.max_turn_requests`).");
            EXIT_FAILURE
        }
        StopReason::LoopDetected | StopReason::ProviderError | StopReason::LogFailed => {
            EXIT_FAILURE
        }
    }
}

fn first_loaded(models: &[ModelInfo]) -> Option<(&ModelInfo, u32)> {
    models.iter().find_map(|model| {
        let instance = model.loaded_instances.first()?;
        Some((model, instance.config.context_length))
    })
}

fn load_context(model: &str, requested: Option<u32>, profiles: &[(String, ProfileLayer)]) -> u32 {
    requested
        .or(praxis_core::agent::profile::resolve(model, None, None, profiles).context)
        .unwrap_or(DEFAULT_CONTEXT)
}

/// Loads the model and returns the model list it read, if it had to read one to pick the model,
/// so the caller doesn't ask LM Studio twice.
async fn ready_model(
    provider: &Provider,
    requested: Option<String>,
    config: &Config,
    terminal: &mut Option<Terminal>,
) -> Result<(ReadyModel, Option<Vec<ModelInfo>>), ProviderError> {
    let (model, context, fetched) = match requested {
        Some(model) => {
            let context = load_context(&model, config.lmstudio.context, &user_profiles(config));
            (model, context, None)
        }
        None => {
            let models = provider.list_models().await?;
            let (key, loaded) = first_loaded(&models)
                .map(|(model, loaded)| (model.key.clone(), loaded))
                .ok_or(ProviderError::NoModelLoaded)?;
            (key, config.lmstudio.context.unwrap_or(loaded), Some(models))
        }
    };
    let ready = load_model(provider, &model, context, config, terminal).await?;
    Ok((ready, fetched))
}

/// Follows SPEC §5.2: the provider refuses an unapproved load, this asks and retries.
async fn load_model(
    provider: &Provider,
    model: &str,
    context: u32,
    config: &Config,
    terminal: &mut Option<Terminal>,
) -> Result<ReadyModel, ProviderError> {
    let policy = if config.agent.auto_load {
        LoadPolicy::Allowed
    } else {
        LoadPolicy::AskFirst
    };
    match provider.ensure_ready(model, context, policy).await {
        Err(error @ ProviderError::LoadNeedsConsent { .. }) => {
            let Some(terminal) = terminal else {
                return Err(error);
            };
            ask(&format!("{error}\nLoad it now? [y/N]: "));
            match terminal.recv().await {
                Some(line) if parse_reply(&line, false) == PermissionReply::AllowOnce => {
                    provider
                        .ensure_ready(model, context, LoadPolicy::Allowed)
                        .await
                }
                _ => Err(error),
            }
        }
        other => other,
    }
}

/// A REPL line that drives plan-first mode (SPEC §11).
#[derive(Debug, PartialEq, Eq)]
enum PlanCommand<'a> {
    /// `/plan <task>`: plan under read-only mode until `/execute`.
    Plan(&'a str),
    /// `/execute`: leave plan mode and carry out the plan the developer has seen.
    Execute,
}

fn plan_command(line: &str) -> Option<PlanCommand<'_>> {
    match line.trim() {
        "/execute" => Some(PlanCommand::Execute),
        "/plan" => Some(PlanCommand::Plan("")),
        other => other
            .strip_prefix("/plan ")
            .map(|task| PlanCommand::Plan(task.trim())),
    }
}

/// Sent with each `/plan` task. The read-only mode is what enforces it; this only asks the
/// model for the right kind of answer.
const PLAN_PROMPT: &str = "Plan mode is on: you can read and search, but change nothing and run \
    no commands. Reply with a plan for the task below and the assumptions it depends on.";
const EXECUTE_PROMPT: &str = "Carry out the plan above now.";

async fn repl(
    mut agent: Agent,
    commands: &[Skill],
    mut terminal: Option<Terminal>,
    interrupt: &Interrupt,
    configured: Mode,
) -> Result<u8> {
    let mut planning = false;
    loop {
        ask(if planning {
            "\npraxis (plan)> "
        } else {
            "\npraxis> "
        });
        let line = match &mut terminal {
            Some(terminal) => terminal.recv().await,
            None => None,
        };
        let Some(line) = line else { return Ok(0) };
        match line.trim() {
            "" => {}
            "/exit" | "/quit" => return Ok(0),
            "/undo" => undo(agent.tools()),
            command if command == "/rewind" || command.starts_with("/rewind ") => {
                rewind(
                    &mut agent,
                    command.get("/rewind".len()..).unwrap_or_default().trim(),
                );
            }
            _ => match plan_command(&line) {
                Some(PlanCommand::Plan("")) => eprintln!("Usage: /plan <task>"),
                Some(PlanCommand::Plan(task)) => {
                    planning = true;
                    agent.set_mode(Mode::ReadOnly);
                    let prompt = format!("{PLAN_PROMPT}\n\n{task}");
                    agent = run_turn(agent, prompt, &mut terminal, interrupt).await?.0;
                }
                Some(PlanCommand::Execute) if !planning => {
                    eprintln!("Nothing to execute; start with /plan <task>.");
                }
                Some(PlanCommand::Execute) => {
                    planning = false;
                    agent.set_mode(configured);
                    let prompt = EXECUTE_PROMPT.to_owned();
                    agent = run_turn(agent, prompt, &mut terminal, interrupt).await?.0;
                }
                None => {
                    let expanded = run_command(
                        commands,
                        line.trim(),
                        agent.tools().workspace(),
                        agent.session_id(),
                    );
                    let prompt = match expanded {
                        Some(prompt) => prompt,
                        // A path such as `/home/me/x.rs fails` is a prompt, not a command.
                        None if line
                            .trim()
                            .strip_prefix('/')
                            .and_then(|rest| rest.split_whitespace().next())
                            .is_some_and(|word| !word.contains('/')) =>
                        {
                            eprintln!("Unknown command: {}", sanitize(line.trim()));
                            continue;
                        }
                        None => line.trim().to_owned(),
                    };
                    agent = run_turn(agent, prompt, &mut terminal, interrupt).await?.0;
                }
            },
        }
    }
}

fn undo(tools: &ToolContext) {
    match tools.undo() {
        Ok(files) if files.is_empty() => eprintln!("Nothing to undo."),
        Ok(files) => {
            for file in files {
                eprintln!("Restored {}", sanitize(&file.display().to_string()));
            }
        }
        Err(e) => eprintln!("Undo failed: {e}"),
    }
    eprintln!("{}", praxis_core::tools::UNDO_NOTE);
}

/// `/rewind` lists the turns it can rewind to; `/rewind <n>` restores files and chat to before turn `n`.
fn rewind(agent: &mut Agent, arg: &str) {
    if arg.is_empty() {
        let turns = agent.rewindable_turns();
        if turns.is_empty() {
            eprintln!("Nothing to rewind to.");
        }
        for (turn, prompt) in turns {
            let first = prompt.lines().next().unwrap_or_default();
            let preview: String = first.chars().take(60).collect();
            eprintln!("{turn}: {}", sanitize(&preview));
        }
        eprintln!("Usage: /rewind <n> restores files and chat to before turn <n>.");
        return;
    }
    let Ok(turn) = arg.parse::<usize>() else {
        eprintln!("Usage: /rewind <n>");
        return;
    };
    match agent.rewind_to_turn(turn) {
        Ok(files) => {
            for file in files {
                eprintln!("Restored {}", sanitize(&file.display().to_string()));
            }
            eprintln!("Chat rewound to before turn {turn}.");
        }
        Err(e) => eprintln!("Rewind failed: {e}"),
    }
    eprintln!("{}", praxis_core::tools::UNDO_NOTE);
}

async fn list_models(provider: &Provider) -> Result<u8> {
    for model in provider.list_models().await? {
        let context = model.loaded_instances.first().map_or_else(
            || format!("not loaded, up to {}", model.max_context_length),
            |instance| format!("loaded with {}", instance.config.context_length),
        );
        let tools = if model.capabilities.trained_for_tool_use {
            ", tools"
        } else {
            ""
        };
        println!("{} ({context}{tools})", sanitize(&model.key));
    }
    Ok(0)
}

/// Sessions are logged when a data directory can be found (SPEC §8).
fn with_session(agent: Agent, resume: Option<Option<&str>>, workspace: &Path) -> Result<Agent> {
    let Some(dir) = xdg_path("XDG_DATA_HOME", ".local/share", "sessions") else {
        eprintln!("No HOME or XDG_DATA_HOME, so this session isn't saved.");
        return Ok(agent);
    };
    let (log, prior) = match resume {
        Some(Some(id)) => resume_log(&dir, id)?,
        Some(None) => {
            let newest = session::list(&dir, workspace)?
                .into_iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("no earlier session in this workspace to resume"))?;
            resume_log(&dir, &newest.id)?
        }
        None => (SessionLog::create(&dir, workspace)?, Vec::new()),
    };
    eprintln!("Session {}", log.id());
    agent
        .tools()
        .checkpoint_into(dir.join(log.id()).join("checkpoints"));
    Ok(agent.with_session(log, prior))
}

fn resume_log(dir: &Path, id: &str) -> Result<(SessionLog, Vec<Message>)> {
    SessionLog::resume(dir, id)
        .map_err(|e| anyhow::anyhow!("can't resume session `{}`: {e}", sanitize(id)))
}

/// The ACP model picker: the models `list_models` reported at startup, loaded on demand.
struct Picker {
    provider: Arc<Provider>,
    config: Config,
    models: Vec<ModelInfo>,
    default: String,
}

impl acp::Models for Picker {
    fn keys(&self) -> Vec<String> {
        self.models.iter().map(|model| model.key.clone()).collect()
    }

    fn default_key(&self) -> String {
        self.default.clone()
    }

    async fn load(&self, key: &str) -> Result<acp::Loaded> {
        let (ready, _) = ready_model(
            &self.provider,
            Some(key.to_owned()),
            &self.config,
            &mut None,
        )
        .await?;
        let capabilities = self
            .models
            .iter()
            .find(|model| model.key == key)
            .map(|model| &model.capabilities);
        let profile = model_profile(&self.config, key, capabilities);
        Ok(acp::Loaded {
            instance_id: ready.instance_id,
            context_size: ready.context_length,
            profile,
        })
    }
}

async fn serve_acp(
    new_agent: acp::NewAgent,
    models: impl acp::Models,
    commands: Vec<Skill>,
) -> Result<u8> {
    let Some(dir) = xdg_path("XDG_DATA_HOME", ".local/share", "sessions") else {
        eprintln!("ACP sessions need HOME or XDG_DATA_HOME to save to.");
        return Ok(EXIT_USAGE);
    };
    acp::agent(new_agent, dir, models, commands)
        .connect_to(agent_client_protocol::Stdio::new())
        .await?;
    Ok(0)
}

async fn run(
    cli: Cli,
    (config, policy): (Config, Policy),
    workspace: PathBuf,
    provider: Provider,
) -> Result<u8> {
    match cli.command {
        Some(Command::Models) => return list_models(&provider).await,
        Some(Command::Plugin(command)) => return plugin_command(command),
        Some(Command::Mcp(McpCommand::List { json })) => {
            return mcp_list(json, &config, &workspace);
        }
        Some(Command::Profile(command)) => {
            return profile_command(command, cli.model, &config, &provider).await;
        }
        Some(Command::Sessions {
            json,
            delete,
            folder,
            new_folder,
            folders,
        }) => {
            return sessions_command(json, delete, folder, new_folder, folders, &workspace);
        }
        Some(Command::Acp) | None => {}
    }
    let acp = matches!(cli.command, Some(Command::Acp));
    // In ACP mode stdin carries the protocol, so nobody can answer at a terminal.
    let interactive = !acp && std::io::stdin().is_terminal();
    let prompt = match (cli.prompt, interactive) {
        (Some(prompt), _) => Some(prompt),
        (None, false) if acp => None,
        (None, true) => None,
        (None, false) => {
            let mut piped = String::new();
            std::io::stdin().read_to_string(&mut piped)?;
            if piped.trim().is_empty() {
                eprintln!("No prompt given. Pass one as an argument or on stdin.");
                return Ok(EXIT_USAGE);
            }
            Some(piped)
        }
    };
    let mut terminal = interactive.then(spawn_terminal_reader);
    let interrupt = Interrupt::install();
    let requested = match cli.model.or_else(|| config.default_model.clone()) {
        Some(reference) => {
            let resolved = config.resolve_enabled_model(&reference)?;
            if resolved.backend == praxis_core::config::Backend::Anthropic {
                // shortcut: no Anthropic client yet; T8.3 adds it.
                eprintln!("`{reference}` is an Anthropic model, which this build can't run yet.");
                return Ok(EXIT_USAGE);
            }
            Some(resolved.model)
        }
        None => None,
    };
    // Plugin discovery and connecting need only config and the workspace, so both overlap the
    // model load (F11). Discovery is blocking file IO, so it runs off the async threads.
    let loading = async {
        let (dir, cfg) = (workspace.clone(), config.clone());
        let plugins = tokio::task::spawn_blocking(move || load_plugins(&dir, &cfg)).await?;
        let mut servers = config.mcp.clone();
        let mut sources = config.mcp_sources.clone();
        sources.extend(
            plugins
                .mcp
                .keys()
                .map(|name| (name.clone(), praxis_core::mcp::McpSource::Plugin)),
        );
        servers.extend(plugins.mcp.clone());
        // Pinning stays after the reply.
        let connected = if servers.is_empty() {
            None
        } else {
            Some(praxis_core::mcp::Mcp::connect(&servers, praxis_core::mcp::CONNECT_TIMEOUT).await)
        };
        Ok::<_, anyhow::Error>((plugins, sources, connected))
    };
    let (ready, loaded) = tokio::join!(
        ready_model(&provider, requested, &config, &mut terminal),
        loading
    );
    let (ready, fetched) = match ready {
        Ok(pair) => pair,
        // Stay up so the editor shows the problem on session/new instead of a dead process.
        Err(error) if acp => {
            let message = error.to_string();
            let picker = Picker {
                provider: Arc::new(provider),
                config,
                models: Vec::new(),
                default: String::new(),
            };
            return serve_acp(
                Box::new(move || Err(anyhow::anyhow!("{message}"))),
                picker,
                Vec::new(),
            )
            .await;
        }
        Err(error) => return Err(error.into()),
    };
    let (plugins, sources, connected) = loaded?;
    let models = match fetched {
        Some(models) => models,
        None => provider.list_models().await?,
    };
    let reported = models.iter().find(|m| {
        m.loaded_instances
            .iter()
            .any(|instance| instance.id == ready.instance_id)
    });
    let key = reported.map_or(ready.instance_id.as_str(), |m| m.key.as_str());
    let default = key.to_owned();
    let profile = model_profile(&config, key, reported.map(|m| &m.capabilities));
    // Plan mode (SPEC §11) switches back to this mode on `/execute`.
    let configured = config.permissions.mode;
    let claude_dir = std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude"));
    let praxis_dir = xdg_dir("XDG_CONFIG_HOME", ".config");
    let trust = xdg_path("XDG_DATA_HOME", ".local/share", "trust.json").map(TrustStore::new);
    let (memory, user_memory) = praxis_core::context::prefix::load_memory_with_user_count(
        &workspace,
        &workspace,
        claude_dir.as_deref(),
        praxis_dir.as_deref(),
        &mut |path, text| approve_memory(trust.as_ref(), path, text, terminal.as_mut()),
    );
    let agent_config = AgentConfig {
        model: ready.instance_id,
        context_size: ready.context_length,
        system_prompt: system_prompt(),
        memory,
        user_memory,
        scoped_rules: praxis_core::context::prefix::load_scoped_rules(
            &workspace,
            claude_dir.as_deref(),
            praxis_dir.as_deref(),
        ),
        mask_keep_turns: config.context.mask_keep_turns,
        max_turn_requests: config.agent.max_turn_requests,
        profile,
        seed: None,
        policy,
        masking: true,
    };
    let mcp = connected.map(|(mcp, mut notices)| {
        let mcp = mcp.with_sources(&sources);
        notices.extend(mcp.pin(trust.as_ref()));
        for notice in notices {
            eprintln!("{}", sanitize(&notice));
        }
        Arc::new(mcp)
    });
    let provider = Arc::new(provider);
    let picker = Picker {
        provider: provider.clone(),
        config: config.clone(),
        models: models.clone(),
        default,
    };
    let session_workspace = workspace.clone();
    let new_agent = move || {
        let tools = Arc::new(tool_context(&workspace, &config)?);
        let agent = Agent::new(provider.clone(), tools, agent_config.clone())
            .with_auto_load(config.auto_loadable_models())
            .with_skills(plugins.skills.clone())
            .with_agents(plugins.agents.clone())
            .with_hooks(plugins.hooks.clone());
        Ok(match &mcp {
            Some(mcp) => agent.with_mcp(mcp.clone()),
            None => agent,
        })
    };
    if acp {
        return until_sigterm(serve_acp(
            Box::new(new_agent),
            picker,
            plugins.commands.clone(),
        ))
        .await;
    }
    let resume = cli.resume.as_ref().map(Option::as_deref);
    let agent = with_session(new_agent()?, resume, &session_workspace)?;
    match prompt {
        Some(prompt) => {
            let prompt = run_command(
                &plugins.commands,
                &prompt,
                agent.tools().workspace(),
                agent.session_id(),
            )
            .unwrap_or(prompt);
            let (_, stop) = run_turn(agent, prompt, &mut terminal, &interrupt).await?;
            Ok(exit_code(stop))
        }
        None => {
            until_sigterm(repl(
                agent,
                &plugins.commands,
                terminal,
                &interrupt,
                configured,
            ))
            .await
        }
    }
}

/// Runs `work` until it ends or the process gets SIGTERM (what VS Code sends to stop ACP).
/// Returning drops the agent, so its MCP servers and tool children are killed; a signal's
/// default action would skip the destructors.
async fn until_sigterm(work: impl Future<Output = Result<u8>>) -> Result<u8> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = work => result,
        _ = terminate.recv() => Ok(0),
    }
}

fn main() -> ExitCode {
    // Hidden from `--help`: it only restricts, then runs what it's told. Handled before the
    // runtime starts, so the sandbox applies to a single-threaded process (SPEC §9).
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if let Some((command, rest)) = args.split_first()
        && command == "sandbox-exec"
    {
        eprintln!("{}", praxis_core::sandbox::exec(rest));
        return ExitCode::from(EXIT_SANDBOX);
    }
    cli_main()
}

#[tokio::main]
async fn cli_main() -> ExitCode {
    // All logs go to stderr: stdout carries the reply (SPEC §11, §15).
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    let mut cli = Cli::parse();
    let workspace = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("Can't read the current directory: {error}");
            return ExitCode::from(EXIT_FAILURE);
        }
    };
    let (config, policy) = match load_config(&workspace, std::mem::take(&mut cli.overrides)) {
        Ok(loaded) => loaded,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    // An unset or non-Unicode variable both just mean no key.
    let api_key = std::env::var(&config.lmstudio.api_key_env).ok();
    let provider = match Provider::new(&config.lmstudio.base_url, api_key) {
        Ok(provider) => provider,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    match run(cli, (config, policy), workspace, provider).await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::from(EXIT_FAILURE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_context_prefers_the_override_then_the_model_then_the_default() {
        assert_eq!(load_context("qwen3.5-4b", Some(4096), &[]), 4096);
        assert_eq!(load_context("qwen3.5-4b", None, &[]), 131_072);
        assert_eq!(
            load_context("some/unknown-model", None, &[]),
            DEFAULT_CONTEXT
        );
        let user = ProfileLayer {
            context: Some(2048),
            ..ProfileLayer::default()
        };
        let profiles = [("qwen3*".to_owned(), user)];
        assert_eq!(load_context("qwen3.5-4b", None, &profiles), 2048);
    }

    #[test]
    fn plan_lines_are_told_apart_from_prompts() {
        assert_eq!(
            plan_command("/plan add a cache"),
            Some(PlanCommand::Plan("add a cache"))
        );
        assert_eq!(
            plan_command("  /plan   spaced  "),
            Some(PlanCommand::Plan("spaced"))
        );
        assert_eq!(plan_command("/plan"), Some(PlanCommand::Plan("")));
        assert_eq!(plan_command("/execute"), Some(PlanCommand::Execute));
        assert_eq!(plan_command("/planning notes"), None);
        assert_eq!(plan_command("write a plan"), None);
    }

    #[test]
    fn a_bare_resume_flag_leaves_the_following_words_as_the_prompt() {
        let cli = Cli::try_parse_from(["praxis", "--resume", "fix bug"]).unwrap();
        assert_eq!(cli.resume, Some(None));
        assert_eq!(cli.prompt.as_deref(), Some("fix bug"));
        let cli = Cli::try_parse_from(["praxis", "--resume=abc", "fix bug"]).unwrap();
        assert_eq!(cli.resume, Some(Some("abc".to_owned())));
        assert_eq!(cli.prompt.as_deref(), Some("fix bug"));
    }

    #[test]
    fn only_explicit_answers_approve() {
        use PermissionReply::{AllowAlways, AllowOnce, RejectAlways, RejectOnce};
        for (answer, can_always, expected) in [
            ("y", true, AllowOnce),
            (" YES ", false, AllowOnce),
            ("a", true, AllowAlways),
            ("a", false, RejectOnce),
            ("r", false, RejectAlways),
            ("", true, RejectOnce),
            ("yep", true, RejectOnce),
        ] {
            assert_eq!(parse_reply(answer, can_always), expected, "{answer}");
        }
    }
}
