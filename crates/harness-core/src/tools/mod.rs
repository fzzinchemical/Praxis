//! Built-in tools and their registry (SPEC §6). Names and parameters are a compatibility
//! surface for plugin hooks: never rename them.

mod bash;
mod confine;
mod files;
mod search;

use crate::provider::ToolSchema;
use schemars::{JsonSchema, generate::SchemaSettings};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

/// The result of one tool call. `structured` is what `PostToolUse` hooks get as `tool_response`.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub structured: Value,
}

impl ToolOutput {
    fn ok(content: String, structured: Value) -> Self {
        Self {
            content,
            is_error: false,
            structured,
        }
    }

    pub fn error(content: String) -> Self {
        Self {
            content,
            is_error: true,
            structured: Value::Null,
        }
    }
}

/// Each message tells the model what happened and what to try next (SPEC §15).
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("Invalid arguments: {0}")]
    InvalidArguments(String),
    #[error("{0} is outside the workspace, so it can't be accessed.")]
    OutsideWorkspace(String),
    #[error("{0} is protected and can't be changed by tools.")]
    HardDenied(String),
    #[error("Can't {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Refused(String),
    #[error("Cancelled.")]
    Cancelled,
}

impl ToolError {
    fn io(action: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.display().to_string();
        move |source| Self::Io {
            action,
            path,
            source,
        }
    }
}

/// Per-session state shared by tool calls.
#[derive(Debug)]
pub struct ToolContext {
    workspace: PathBuf,
    /// The already scrubbed environment for child processes.
    env: Vec<(OsString, OsString)>,
    /// Content hash of each file as last read or written, so `Edit` can spot outside changes.
    seen: Mutex<HashMap<PathBuf, u64>>,
}

impl ToolContext {
    /// Fails if `workspace` can't be resolved: confinement needs its real path.
    pub fn new(workspace: &Path) -> Result<Self, ToolError> {
        Self::with_env(workspace, std::env::vars_os())
    }

    /// Like [`Self::new`], scrubbing `vars` instead of this process's environment.
    pub fn with_env(
        workspace: &Path,
        vars: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Result<Self, ToolError> {
        Ok(Self {
            workspace: workspace
                .canonicalize()
                .map_err(ToolError::io("open the workspace", workspace))?,
            env: bash::child_env(vars),
            seen: Mutex::default(),
        })
    }

    fn remember(&self, path: &Path, hash: u64) {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path.to_owned(), hash);
    }

    fn last_seen(&self, path: &Path) -> Option<u64> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(path)
            .copied()
    }
}

fn schema_of<T: JsonSchema>() -> Value {
    let settings = SchemaSettings::draft2020_12().with(|s| s.meta_schema = None);
    let mut schema = settings
        .into_generator()
        .into_root_schema_for::<T>()
        .to_value();
    if let Some(object) = schema.as_object_mut() {
        object.remove("title");
    }
    schema
}

/// Schemas in a fixed order: any change to them invalidates the server's prompt cache (SPEC §7).
pub fn schemas() -> Vec<ToolSchema> {
    let tool = |name: &str, description: &str, parameters: Value| ToolSchema {
        name: name.to_owned(),
        description: description.to_owned(),
        parameters,
    };
    vec![
        tool(
            "Read",
            "Read a text file as numbered lines. Required before Edit.",
            schema_of::<files::ReadInput>(),
        ),
        tool(
            "Write",
            "Create a file or replace its whole content.",
            schema_of::<files::WriteInput>(),
        ),
        tool(
            "Edit",
            "Replace an exact string in a file you have read.",
            schema_of::<files::EditInput>(),
        ),
        tool(
            "Bash",
            "Run a shell command in the workspace. Non-interactive, with a scrubbed environment.",
            schema_of::<bash::BashInput>(),
        ),
        tool(
            "Grep",
            "Search file contents with a regex. Skips hidden and ignored files.",
            schema_of::<search::GrepInput>(),
        ),
        tool(
            "Glob",
            "List files matching a glob such as `**/*.rs`. Skips hidden and ignored files.",
            schema_of::<search::GlobInput>(),
        ),
    ]
}

/// Checks arguments against the tool's schema (SPEC §5.3). `Err` names the problem and the
/// expected shape, for the model to correct.
pub fn validate(name: &str, input: &Value) -> Result<(), String> {
    let all = schemas();
    let Some(schema) = all.iter().find(|tool| tool.name == name) else {
        let known: Vec<&str> = all.iter().map(|tool| tool.name.as_str()).collect();
        return Err(format!(
            "Unknown tool `{name}`. Expected one of: {}.",
            known.join(", ")
        ));
    };
    let validator = jsonschema::validator_for(&schema.parameters)
        .map_err(|e| format!("The schema for `{name}` is broken: {e}"))?;
    let problems: Vec<String> = validator
        .iter_errors(input)
        .map(|e| e.to_string())
        .collect();
    if problems.is_empty() {
        return Ok(());
    }
    Err(format!(
        "Invalid arguments for `{name}`: {}. Expected: {}",
        problems.join("; "),
        schema.parameters
    ))
}

fn parse<T: DeserializeOwned>(input: Value) -> Result<T, ToolError> {
    serde_json::from_value(input).map_err(|e| ToolError::InvalidArguments(e.to_string()))
}

fn run_blocking(
    name: &str,
    input: Value,
    ctx: &ToolContext,
    cancel: &CancellationToken,
) -> Result<ToolOutput, ToolError> {
    match name {
        "Read" => files::read(parse(input)?, ctx),
        "Write" => files::write(parse(input)?, ctx),
        "Edit" => files::edit(parse(input)?, ctx),
        "Grep" => search::grep(parse(input)?, ctx, cancel),
        "Glob" => search::glob(parse(input)?, ctx, cancel),
        other => Err(ToolError::Refused(format!("Unknown tool `{other}`."))),
    }
}

/// Runs a tool. Failures, including a panic inside the tool, come back as content for the
/// model and never end the turn (SPEC §6, §15).
pub async fn execute(
    name: &str,
    input: Value,
    ctx: Arc<ToolContext>,
    cancel: CancellationToken,
) -> ToolOutput {
    let name = name.to_owned();
    let task = if name == "Bash" {
        tokio::spawn(async move { bash::run(parse(input)?, &ctx, &cancel).await })
    } else {
        tokio::task::spawn_blocking(move || run_blocking(&name, input, &ctx, &cancel))
    };
    match task.await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => ToolOutput::error(error.to_string()),
        Err(_) => ToolOutput::error("The tool crashed. Try a different approach.".to_owned()),
    }
}
