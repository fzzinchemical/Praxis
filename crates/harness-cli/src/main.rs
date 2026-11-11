#![forbid(unsafe_code)]

//! The `harness` binary: one-shot runs, the REPL and `harness models` (SPEC §11).

use anyhow::Result;
use clap::{Parser, Subcommand};
use harness_core::agent::{
    Agent, AgentConfig, AgentEvent, DEFAULT_MAX_TURN_REQUESTS, PermissionReply, StopReason,
    ToolOutcome, system_prompt,
};
use harness_core::context::compaction::DEFAULT_MASK_KEEP_TURNS;
use harness_core::provider::{LoadPolicy, ModelInfo, Provider, ProviderError, ReadyModel};
use harness_core::tools::ToolContext;
use serde_json::Value;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

const DEFAULT_BASE_URL: &str = "http://localhost:1234";
// For models with no built-in context in their profile.
const DEFAULT_CONTEXT: u32 = 32768;
const API_KEY_ENV: &str = "LM_API_TOKEN";
const EXIT_FAILURE: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_CANCELLED: u8 = 130;

#[derive(Parser)]
#[command(name = "harness", about = "Local-first coding agent for LM Studio")]
struct Cli {
    /// Run this prompt and exit. Without it, reads stdin if piped, otherwise starts a REPL.
    prompt: Option<String>,
    /// Model key. Defaults to the first model LM Studio has loaded.
    #[arg(long, global = true)]
    model: Option<String>,
    /// base_url, context or auto_load, as KEY=VALUE.
    #[arg(long = "config-override", value_name = "KEY=VALUE", global = true)]
    overrides: Vec<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List models and their effective context.
    Models,
}

// shortcut: only the keys T1.9 needs, from flags alone; config files arrive with the config task.
struct Settings {
    base_url: String,
    context: Option<u32>,
    auto_load: bool,
}

fn parse_settings(overrides: &[String]) -> Result<Settings, String> {
    let mut settings = Settings {
        base_url: DEFAULT_BASE_URL.to_owned(),
        context: None,
        auto_load: false,
    };
    for entry in overrides {
        let invalid = || format!("Invalid --config-override `{entry}`.");
        let (key, value) = entry.split_once('=').ok_or_else(invalid)?;
        match key {
            "base_url" => value.clone_into(&mut settings.base_url),
            "context" => settings.context = Some(value.parse().map_err(|_| invalid())?),
            "auto_load" => settings.auto_load = value.parse().map_err(|_| invalid())?,
            _ => return Err(format!("Unknown config key `{key}`.")),
        }
    }
    Ok(settings)
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
            eprintln!("Stopped at the limit of {DEFAULT_MAX_TURN_REQUESTS} requests in one turn.");
            EXIT_FAILURE
        }
        StopReason::LoopDetected | StopReason::ProviderError => EXIT_FAILURE,
    }
}

fn first_loaded(models: &[ModelInfo]) -> Option<(&ModelInfo, u32)> {
    models.iter().find_map(|model| {
        let instance = model.loaded_instances.first()?;
        Some((model, instance.config.context_length))
    })
}

fn load_context(model: &str, requested: Option<u32>) -> u32 {
    requested
        .or(harness_core::agent::profile::resolve(model, None, &[]).context)
        .unwrap_or(DEFAULT_CONTEXT)
}

/// Follows SPEC §5.2: the provider refuses an unapproved load, this asks and retries.
async fn ready_model(
    provider: &Provider,
    requested: Option<String>,
    settings: &Settings,
    terminal: &mut Option<Terminal>,
) -> Result<ReadyModel, ProviderError> {
    let (model, context) = match requested {
        Some(model) => {
            let context = load_context(&model, settings.context);
            (model, context)
        }
        None => {
            let models = provider.list_models().await?;
            let (model, loaded) = first_loaded(&models).ok_or(ProviderError::NoModelLoaded)?;
            (model.key.clone(), settings.context.unwrap_or(loaded))
        }
    };
    let policy = if settings.auto_load {
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

async fn repl(
    mut agent: Agent,
    mut terminal: Option<Terminal>,
    interrupt: &Interrupt,
) -> Result<u8> {
    loop {
        ask("\nharness> ");
        let line = match &mut terminal {
            Some(terminal) => terminal.recv().await,
            None => None,
        };
        let Some(line) = line else { return Ok(0) };
        match line.trim() {
            "" => {}
            "/exit" | "/quit" => return Ok(0),
            prompt => {
                agent = run_turn(agent, prompt.to_owned(), &mut terminal, interrupt)
                    .await?
                    .0;
            }
        }
    }
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

async fn run(cli: Cli, settings: Settings, provider: Provider) -> Result<u8> {
    if matches!(cli.command, Some(Command::Models)) {
        return list_models(&provider).await;
    }
    let interactive = std::io::stdin().is_terminal();
    let prompt = match (cli.prompt, interactive) {
        (Some(prompt), _) => Some(prompt),
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
    let ready = ready_model(&provider, cli.model, &settings, &mut terminal).await?;
    let models = provider.list_models().await?;
    let reported = models.iter().find(|m| {
        m.loaded_instances
            .iter()
            .any(|instance| instance.id == ready.instance_id)
    });
    let key = reported.map_or(ready.instance_id.as_str(), |m| m.key.as_str());
    // shortcut: no user layer until config arrives in M3, which must also reach `load_context`.
    let profile =
        harness_core::agent::profile::resolve(key, reported.map(|m| &m.capabilities), &[]);
    let workspace = std::env::current_dir()?;
    let agent = Agent::new(
        Arc::new(provider),
        Arc::new(ToolContext::new(&workspace)?),
        AgentConfig {
            model: ready.instance_id,
            context_size: ready.context_length,
            system_prompt: system_prompt(&workspace),
            agents_md: None,
            mask_keep_turns: DEFAULT_MASK_KEEP_TURNS,
            max_turn_requests: DEFAULT_MAX_TURN_REQUESTS,
            profile,
            seed: None,
            masking: true,
        },
    );
    match prompt {
        Some(prompt) => {
            let (_, stop) = run_turn(agent, prompt, &mut terminal, &interrupt).await?;
            Ok(exit_code(stop))
        }
        None => repl(agent, terminal, &interrupt).await,
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    // All logs go to stderr: stdout carries the reply (SPEC §11, §15).
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let settings = match parse_settings(&cli.overrides) {
        Ok(settings) => settings,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    // An unset or non-Unicode variable both just mean no key.
    let api_key = std::env::var(API_KEY_ENV).ok();
    let provider = match Provider::new(&settings.base_url, api_key) {
        Ok(provider) => provider,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    match run(cli, settings, provider).await {
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
        assert_eq!(load_context("qwen3.5-4b", Some(4096)), 4096);
        assert_eq!(load_context("qwen3.5-4b", None), 131_072);
        assert_eq!(load_context("some/unknown-model", None), DEFAULT_CONTEXT);
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
