#![forbid(unsafe_code)]

//! The `harness` binary: one-shot runs, the REPL, `harness models` and `harness acp` (SPEC §11).

mod acp;

use agent_client_protocol::ConnectTo;
use anyhow::Result;
use clap::{Parser, Subcommand};
use harness_core::agent::profile::ProfileLayer;
use harness_core::agent::{
    Agent, AgentConfig, AgentEvent, PermissionReply, StopReason, ToolOutcome, system_prompt,
};
use harness_core::config::McpServer;
use harness_core::config::{self, Config, SandboxMode, Sources};
use harness_core::hooks::Hook;
use harness_core::permissions::{Mode, Policy};
use harness_core::plugin::{AgentDef, Skill, Store, run_command, standalone, standalone_agents};
use harness_core::provider::{LoadPolicy, Message, ModelInfo, Provider, ProviderError, ReadyModel};
use harness_core::sandbox::Sandbox;
use harness_core::session::{self, SessionLog};
use harness_core::summary::summarize;
use harness_core::tools::ToolContext;
use harness_core::trust::TrustStore;
use serde_json::Value;
use std::collections::BTreeMap;
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
#[command(name = "harness", about = "Local-first coding agent for LM Studio")]
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
    #[arg(long, value_name = "ID", num_args = 0..=1)]
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
    /// List this workspace's saved sessions, newest first. Starts no model or MCP server.
    Sessions {
        /// Print a JSON array of `{id, title, started, last_active}`.
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
    /// Remove the plugin. Its data is kept unless removal is confirmed separately.
    Remove {
        name: String,
        /// Also delete `plugins/data/<name>/` without asking.
        #[arg(long)]
        delete_data: bool,
    },
}

/// `harness plugin ...`: no model or server needed.
fn plugin_command(command: PluginCommand) -> Result<u8> {
    let root = xdg_path("XDG_DATA_HOME", ".local/share", "plugins")
        .ok_or_else(|| anyhow::anyhow!("no XDG_DATA_HOME or HOME to install plugins under"))?;
    std::fs::create_dir_all(&root)?;
    let store = harness_core::plugin::Store { root };
    let show = |plugin: &harness_core::plugin::Plugin| -> Result<()> {
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
            println!("Run `harness plugin enable {}` to enable it.", plugin.name);
        }
        PluginCommand::List { json: true } => {
            let mut plugins = Vec::new();
            for (name, entry) in store.list().map_err(anyhow::Error::msg)? {
                let (_, plugin) = store.get(&name).map_err(anyhow::Error::msg)?;
                let can_run = plugin
                    .runs(&store.data_root())
                    .map_err(anyhow::Error::msg)?;
                plugins.push(serde_json::json!({
                    "name": name,
                    "source": entry.source,
                    "commit": entry.commit,
                    "enabled": entry.enabled,
                    "can_run": can_run,
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

/// `harness sessions`: reads the session logs only, so the sidebar never starts a model or MCP server.
fn sessions_command(json: bool, workspace: &Path) -> Result<u8> {
    let Some(dir) = xdg_path("XDG_DATA_HOME", ".local/share", "sessions") else {
        eprintln!("Sessions need HOME or XDG_DATA_HOME to read from.");
        return Ok(EXIT_USAGE);
    };
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
        out.skills.extend(plugin.skills());
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

/// `~/.config/harness` then `~/.claude`, highest precedence first.
fn user_layers() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => home.as_ref().map(|h| h.join(".config")),
    };
    [
        config.map(|c| c.join("harness")),
        home.map(|h| h.join(".claude")),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// `.harness` then `.claude` in the workspace, highest precedence first.
fn project_layers(workspace: &Path) -> Vec<PathBuf> {
    vec![workspace.join(".harness"), workspace.join(".claude")]
}

/// `$<xdg_var>/harness/<file>`, else `~/<fallback>/harness/<file>`.
fn xdg_path(xdg_var: &str, fallback: &str, file: &str) -> Option<PathBuf> {
    xdg_dir(xdg_var, fallback).map(|dir| dir.join(file))
}

/// `$<xdg_var>/harness`, else `~/<fallback>/harness`.
fn xdg_dir(xdg_var: &str, fallback: &str) -> Option<PathBuf> {
    let base = match std::env::var_os(xdg_var) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(fallback),
    };
    Some(base.join("harness"))
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

/// A changed MCP tool stays hidden unless a person at a terminal approves it (SPEC §13).
fn approve_mcp_change(change: &str, terminal: Option<&mut Terminal>) -> bool {
    let Some(terminal) = terminal else {
        return false;
    };
    ask(&format!(
        "\n{}\nApprove this tool? [y/N]: ",
        sanitize(change)
    ));
    // The terminal reader owns stdin, so wait on its channel from this sync callback.
    tokio::task::block_in_place(|| terminal.blocking_recv())
        .is_some_and(|line| parse_reply(&line, false) == PermissionReply::AllowOnce)
}

fn load_config(workspace: &Path, overrides: Vec<String>) -> Result<(Config, Policy), String> {
    let sources = Sources {
        user: xdg_path("XDG_CONFIG_HOME", ".config", "config.toml"),
        project: Some(workspace.join(".harness/config.toml")),
        mcp_json: Some(workspace.join(".mcp.json")),
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
    let tools = ToolContext::new(workspace)?.with_output_filter(config.context.filter_output);
    let settings = &config.sandbox;
    if settings.mode == SandboxMode::Off {
        eprintln!("The sandbox is off: commands run with your full access.");
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
        match event {
            AgentEvent::TextDelta(text) => {
                let text = sanitize(&text);
                mid_line = !text.ends_with('\n');
                stdout.write_all(text.as_bytes())?;
                stdout.flush()?;
            }
            // Reasoning stays out of the main view (SPEC §11).
            AgentEvent::ThinkingDelta(_) => {}
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
        .or(harness_core::agent::profile::resolve(model, None, profiles).context)
        .unwrap_or(DEFAULT_CONTEXT)
}

/// Follows SPEC §5.2: the provider refuses an unapproved load, this asks and retries.
async fn ready_model(
    provider: &Provider,
    requested: Option<String>,
    config: &Config,
    terminal: &mut Option<Terminal>,
) -> Result<ReadyModel, ProviderError> {
    let (model, context) = match requested {
        Some(model) => {
            let context = load_context(&model, config.lmstudio.context, &user_profiles(config));
            (model, context)
        }
        None => {
            let models = provider.list_models().await?;
            let (model, loaded) = first_loaded(&models).ok_or(ProviderError::NoModelLoaded)?;
            (model.key.clone(), config.lmstudio.context.unwrap_or(loaded))
        }
    };
    let policy = if config.agent.auto_load {
        LoadPolicy::Allowed
    } else {
        LoadPolicy::AskFirst
    };
    match provider.ensure_ready(&model, context, policy).await {
        Err(error @ ProviderError::LoadNeedsConsent { .. }) => {
            let Some(terminal) = terminal else {
                return Err(error);
            };
            ask(&format!("{error}\nLoad it now? [y/N]: "));
            match terminal.recv().await {
                Some(line) if parse_reply(&line, false) == PermissionReply::AllowOnce => {
                    provider
                        .ensure_ready(&model, context, LoadPolicy::Allowed)
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
            "\nharness (plan)> "
        } else {
            "\nharness> "
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
            _ => match plan_command(&line) {
                Some(PlanCommand::Plan("")) => eprintln!("Usage: /plan <task>"),
                Some(PlanCommand::Plan(task)) => {
                    planning = true;
                    agent.set_mode(Mode::ReadOnly);
                    let prompt = format!("{PLAN_PROMPT}\n\n{task}");
                    agent = run_turn(agent, prompt, &mut terminal, interrupt).await?.0;
                }
                Some(PlanCommand::Execute) => {
                    planning = false;
                    agent.set_mode(configured);
                    let prompt = EXECUTE_PROMPT.to_owned();
                    agent = run_turn(agent, prompt, &mut terminal, interrupt).await?.0;
                }
                None => {
                    let prompt = run_command(
                        commands,
                        line.trim(),
                        agent.tools().workspace(),
                        agent.session_id(),
                    )
                    .unwrap_or_else(|| line.trim().to_owned());
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
    eprintln!("{}", harness_core::tools::UNDO_NOTE);
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
        let ready = ready_model(
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
        let profile =
            harness_core::agent::profile::resolve(key, capabilities, &user_profiles(&self.config));
        Ok(acp::Loaded {
            instance_id: ready.instance_id,
            context_size: ready.context_length,
            profile,
        })
    }
}

async fn serve_acp(new_agent: acp::NewAgent, models: impl acp::Models) -> Result<u8> {
    let Some(dir) = xdg_path("XDG_DATA_HOME", ".local/share", "sessions") else {
        eprintln!("ACP sessions need HOME or XDG_DATA_HOME to save to.");
        return Ok(EXIT_USAGE);
    };
    acp::agent(new_agent, dir, models)
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
        Some(Command::Sessions { json }) => return sessions_command(json, &workspace),
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
            if resolved.backend == harness_core::config::Backend::Anthropic {
                // shortcut: no Anthropic client yet; T8.3 adds it.
                eprintln!("`{reference}` is an Anthropic model, which this build can't run yet.");
                return Ok(EXIT_USAGE);
            }
            Some(resolved.model)
        }
        None => None,
    };
    let ready = match ready_model(&provider, requested, &config, &mut terminal).await {
        Ok(ready) => ready,
        // Stay up so the editor shows the problem on session/new instead of a dead process.
        Err(error) if acp => {
            let message = error.to_string();
            let picker = Picker {
                provider: Arc::new(provider),
                config,
                models: Vec::new(),
                default: String::new(),
            };
            return serve_acp(Box::new(move || Err(anyhow::anyhow!("{message}"))), picker).await;
        }
        Err(error) => return Err(error.into()),
    };
    let models = provider.list_models().await?;
    let reported = models.iter().find(|m| {
        m.loaded_instances
            .iter()
            .any(|instance| instance.id == ready.instance_id)
    });
    let key = reported.map_or(ready.instance_id.as_str(), |m| m.key.as_str());
    let default = key.to_owned();
    let profile = harness_core::agent::profile::resolve(
        key,
        reported.map(|m| &m.capabilities),
        &user_profiles(&config),
    );
    // Plan mode (SPEC §11) switches back to this mode on `/execute`.
    let configured = config.permissions.mode;
    let claude_dir = std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude"));
    let harness_dir = xdg_dir("XDG_CONFIG_HOME", ".config");
    let agent_config = AgentConfig {
        model: ready.instance_id,
        context_size: ready.context_length,
        system_prompt: system_prompt(&workspace),
        memory: harness_core::context::prefix::load_memory(
            &workspace,
            &workspace,
            claude_dir.as_deref(),
            harness_dir.as_deref(),
        ),
        scoped_rules: harness_core::context::prefix::load_scoped_rules(
            &workspace,
            claude_dir.as_deref(),
            harness_dir.as_deref(),
        ),
        mask_keep_turns: config.context.mask_keep_turns,
        max_turn_requests: config.agent.max_turn_requests,
        profile,
        seed: None,
        policy,
        masking: true,
    };
    let plugins = load_plugins(&workspace, &config);
    let mut servers = config.mcp.clone();
    servers.extend(plugins.mcp);
    let mcp = if servers.is_empty() {
        None
    } else {
        let (mut mcp, mut notices) =
            harness_core::mcp::Mcp::connect(&servers, harness_core::mcp::CONNECT_TIMEOUT).await;
        let store = xdg_path("XDG_DATA_HOME", ".local/share", "trust.json").map(TrustStore::new);
        notices.extend(mcp.pin(store.as_ref(), &mut |change| {
            approve_mcp_change(change, terminal.as_mut())
        }));
        for notice in notices {
            eprintln!("{}", sanitize(&notice));
        }
        Some(Arc::new(mcp))
    };
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
            .with_skills(plugins.skills.clone())
            .with_agents(plugins.agents.clone())
            .with_hooks(plugins.hooks.clone());
        Ok(match &mcp {
            Some(mcp) => agent.with_mcp(mcp.clone()),
            None => agent,
        })
    };
    if acp {
        return serve_acp(Box::new(new_agent), picker).await;
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
        None => repl(agent, &plugins.commands, terminal, &interrupt, configured).await,
    }
}

fn main() -> ExitCode {
    // Hidden from `--help`: it only restricts, then runs what it's told. Handled before the
    // runtime starts, so the sandbox applies to a single-threaded process (SPEC §9).
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if let Some((command, rest)) = args.split_first()
        && command == "sandbox-exec"
    {
        eprintln!("{}", harness_core::sandbox::exec(rest));
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
