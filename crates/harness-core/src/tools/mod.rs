//! Built-in tools and their registry (SPEC §6). Names and parameters are a compatibility
//! surface for plugin hooks: never rename them.

pub(crate) mod bash;
mod checkpoint;
pub(crate) mod confine;
mod files;
mod filter;
mod search;

pub use checkpoint::UNDO_NOTE;
pub use files::{FileDiff, proposed_diff};

use crate::provider::ToolSchema;
use schemars::{JsonSchema, generate::SchemaSettings};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};
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

type ReadFn = dyn Fn(&Path) -> Option<String> + Send + Sync;

/// Returns an editor's text for an absolute path, unsaved edits included, or `None` to read disk.
pub struct EditorReader(pub Box<ReadFn>);

impl std::fmt::Debug for EditorReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EditorReader")
    }
}

/// Per-session state shared by tool calls.
#[derive(Debug)]
pub struct ToolContext {
    workspace: PathBuf,
    /// Set by the ACP front end when the client can serve `fs/read_text_file` (SPEC §11).
    editor: OnceLock<EditorReader>,
    /// The already scrubbed environment for child processes.
    env: Vec<(OsString, OsString)>,
    /// Content hash of each file as last read or written, so `Edit` can spot outside changes.
    seen: Mutex<HashMap<PathBuf, u64>>,
    /// Holds this session's full oversized results (SPEC §7). Created on first use.
    spool_dir: PathBuf,
    /// How many files are in `spool_dir`, or `None` until this session has created it.
    spooled: Mutex<Option<u64>>,
    /// `None` runs `Bash` unsandboxed, which only `sandbox.mode = "off"` should choose.
    sandbox: Option<crate::sandbox::Sandbox>,
    /// Whether `Bash` output is filtered before the model sees it (SPEC §7).
    filter_output: bool,
    /// `Bash`'s private, writable `TMPDIR`. Created on first use.
    tmp_dir: PathBuf,
    tmp_created: Mutex<bool>,
    checkpoints: Mutex<checkpoint::Checkpoints>,
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
            editor: OnceLock::new(),
            env: bash::child_env(vars),
            seen: Mutex::default(),
            spool_dir: new_temp_path("spool"),
            spooled: Mutex::default(),
            sandbox: None,
            filter_output: false,
            tmp_dir: new_temp_path("tmp"),
            tmp_created: Mutex::default(),
            checkpoints: Mutex::default(),
        })
    }

    /// Lets `Read` see what the editor shows. Set once, before the session's first turn.
    pub fn read_from_editor(&self, reader: EditorReader) {
        // shortcut: a second call keeps the first reader; the ACP front end sets it once per session.
        let _ = self.editor.set(reader);
    }

    /// The editor's text for `path`, if the client serves it.
    fn editor_text(&self, path: &Path) -> Option<String> {
        (self.editor.get()?.0)(path)
    }

    /// Runs every `Bash` command through `sandbox`'s helper (SPEC §9).
    pub fn with_sandbox(mut self, sandbox: crate::sandbox::Sandbox) -> Self {
        self.sandbox = Some(sandbox);
        self
    }

    /// Filters `Bash` output for the model; the full text goes to the spool when it changes.
    pub fn with_output_filter(mut self, on: bool) -> Self {
        self.filter_output = on;
        self
    }

    /// This session's private temp folder for commands, created on first use.
    fn tmp(&self) -> io::Result<&Path> {
        let mut created = self
            .tmp_created
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !*created {
            DirBuilder::new().mode(0o700).create(&self.tmp_dir)?;
            *created = true;
        }
        Ok(&self.tmp_dir)
    }

    /// Saves `text` privately and returns its path. The name is ours alone: tool call ids come
    /// from the model, so they must never reach a path.
    pub fn spool(&self, text: &str) -> io::Result<PathBuf> {
        let mut spooled = self.spooled.lock().unwrap_or_else(PoisonError::into_inner);
        let count = match *spooled {
            Some(count) => count,
            None => {
                // `create` fails if the path exists, so nobody can plant a directory they can read.
                DirBuilder::new().mode(0o700).create(&self.spool_dir)?;
                0
            }
        };
        *spooled = Some(count + 1);
        let path = self.spool_dir.join(format!("result-{}.txt", count + 1));
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?
            .write_all(text.as_bytes())?;
        Ok(path)
    }

    /// Like [`confine::resolve`], but also lets `Read` reach this session's spool.
    fn resolve_read(&self, raw: &str) -> Result<PathBuf, ToolError> {
        match fs::canonicalize(raw) {
            Ok(path) if Path::new(raw).is_absolute() && path.starts_with(&self.spool_dir) => {
                Ok(path)
            }
            _ => confine::resolve(&self.workspace, raw),
        }
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    fn remember(&self, path: &Path, hash: u64) {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path.to_owned(), hash);
    }

    fn forget(&self, path: &Path) {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(path);
    }

    fn last_seen(&self, path: &Path) -> Option<u64> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(path)
            .copied()
    }
}

impl Drop for ToolContext {
    fn drop(&mut self) {
        let spooled = self
            .spooled
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        if spooled.is_some()
            && let Err(error) = fs::remove_dir_all(&self.spool_dir)
        {
            tracing::warn!(%error, "couldn't remove the spool directory");
        }
        let tmp_created = *self
            .tmp_created
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        if tmp_created && let Err(error) = fs::remove_dir_all(&self.tmp_dir) {
            tracing::warn!(%error, "couldn't remove the command temp directory");
        }
    }
}

// shortcut: spools live in the temp directory and a killed process leaves them behind;
// move them beside the session log when sessions arrive.
/// A real path, so it compares equal to canonicalized paths. Unique per context.
fn new_temp_path(kind: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let temp = std::env::temp_dir();
    let temp = temp.canonicalize().unwrap_or(temp);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    temp.join(format!(
        "harness-{kind}-{}-{nanos}-{id}",
        std::process::id()
    ))
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
            "Search file contents with a regex. Skips hidden and ignored files. Shows matching lines with line numbers by default; set `output_mode` to `files_with_matches` to list files only.",
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
    validate_schema(schema, input)
}

/// Like [`validate`], against a schema from elsewhere, such as an MCP server.
pub(crate) fn validate_schema(schema: &ToolSchema, input: &Value) -> Result<(), String> {
    let name = &schema.name;
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
