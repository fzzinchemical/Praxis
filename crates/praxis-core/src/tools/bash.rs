//! `Bash`, run through the sandbox helper unless the user turned the sandbox off (SPEC §9).
//!
//! shortcut: a command that calls `setsid` leaves the process group and survives the kill; use a
//! cgroup or PID namespace if that becomes a real problem.

use super::{ToolContext, ToolError, ToolOutput, filter};
use crate::sandbox::Policy;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
/// How long to wait for output after the kill: a process that escaped the group may hold the pipe.
const DRAIN_GRACE: Duration = Duration::from_secs(2);
/// Per stream; the first and last halves are kept, since errors usually come last (SPEC §7).
// shortcut: the middle of a huge stream is dropped before spooling sees it; spool while reading if the middle matters.
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

/// An allowlist, so a secret under an unexpected name stays out too (SPEC §9, §10).
const ALLOWED_ENV: [&str; 14] = [
    "PATH",
    "HOME",
    "USER",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "TMPDIR",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "GOPATH",
    "GOROOT",
    "JAVA_HOME",
    "VIRTUAL_ENV",
];

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BashInput {
    command: String,
    /// What the command does, in a few words.
    #[expect(
        dead_code,
        reason = "part of the Claude Code schema; only hooks read it"
    )]
    description: Option<String>,
    /// Timeout in milliseconds. Defaults to 120000, at most 600000.
    timeout: Option<u64>,
}

pub fn child_env(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    vars.into_iter()
        .filter(|(name, _)| ALLOWED_ENV.iter().any(|allowed| name == allowed))
        .collect()
}

#[derive(Default)]
struct Captured {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    omitted: u64,
}

impl Captured {
    fn push(&mut self, mut bytes: &[u8]) {
        let half = MAX_CAPTURE_BYTES / 2;
        let into_head = bytes.len().min(half - self.head.len());
        self.head.extend_from_slice(&bytes[..into_head]);
        bytes = &bytes[into_head..];
        let overflow = (self.tail.len() + bytes.len()).saturating_sub(half);
        let from_tail = overflow.min(self.tail.len());
        self.tail.drain(..from_tail);
        self.omitted += overflow as u64;
        self.tail.extend(&bytes[overflow - from_tail..]);
    }

    /// Lossy, so a character cut at either edge of the gap shows as U+FFFD.
    fn text(&self) -> String {
        let head = String::from_utf8_lossy(&self.head);
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        let tail = String::from_utf8_lossy(&tail);
        match self.omitted {
            0 => format!("{head}{tail}"),
            n => format!("{head}\n[output truncated: {n} bytes omitted]\n{tail}"),
        }
    }
}

/// Keeps reading past the cap so the child never blocks on a full pipe.
async fn capture(mut pipe: impl AsyncRead + Unpin, stop: CancellationToken) -> Captured {
    let mut captured = Captured::default();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = tokio::select! {
            read = pipe.read(&mut chunk) => read,
            () = stop.cancelled() => break,
        };
        let Ok(n @ 1..) = read else { break };
        captured.push(&chunk[..n]);
    }
    captured
}

/// Signalling a group needs `kill(2)`, which is `unsafe` from Rust and forbidden in this crate,
/// so this asks the shell's builtin. Returns whether the group got the signal.
async fn kill_group(pgid: u32) -> bool {
    Command::new("/bin/sh")
        .args(["-c", "kill -s KILL -- \"-$1\"", "sh", &pgid.to_string()])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

enum Ending {
    Exited(std::io::Result<ExitStatus>),
    TimedOut(u64),
    Cancelled,
}

/// Shows the filtered text and keeps the full one in the spool. If the spool fails, the full
/// text is shown instead, so filtering never loses output.
fn filtered(ctx: &ToolContext, full: String) -> String {
    let kept = filter::filter(&full);
    if kept == full {
        return full;
    }
    match ctx.spool(&full) {
        Ok(path) => format!(
            "{kept}[Output filtered to save context. Full output: {}. Read it for the rest.]",
            path.display()
        ),
        Err(error) => {
            tracing::warn!(%error, "couldn't spool filtered output; showing it unfiltered");
            full
        }
    }
}

pub async fn run(
    input: BashInput,
    ctx: &ToolContext,
    cancel: &CancellationToken,
) -> Result<ToolOutput, ToolError> {
    let timeout_ms = input
        .timeout
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .min(MAX_TIMEOUT_MS);
    let tmp = ctx.tmp().map_err(ToolError::io("create", &ctx.tmp_dir))?;
    let shell = ["/bin/sh", "-c", &input.command];
    let mut command = match &ctx.sandbox {
        Some(sandbox) => {
            let policy = Policy::new(sandbox, &ctx.workspace, tmp, &ctx.env);
            let policy = serde_json::to_string(&policy)
                .map_err(|e| ToolError::Refused(format!("Can't encode the sandbox policy: {e}")))?;
            let mut command = Command::new(&sandbox.helper);
            command
                .args(["sandbox-exec", "--policy", &policy, "--"])
                .args(shell);
            command
        }
        None => {
            let mut command = Command::new(shell[0]);
            command.args(&shell[1..]);
            command
        }
    };
    let mut child = command
        .current_dir(&ctx.workspace)
        .env_clear()
        .envs(ctx.env.iter().map(|(name, value)| (name, value)))
        .env("TMPDIR", tmp)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own group, so the kill reaches grandchildren and spares this process.
        .process_group(0)
        .spawn()
        .map_err(ToolError::io("start", "/bin/sh".as_ref()))?;
    let pgid = child.id();

    let stop = CancellationToken::new();
    let stdout = child
        .stdout
        .take()
        .map(|pipe| tokio::spawn(capture(pipe, stop.clone())));
    let stderr = child
        .stderr
        .take()
        .map(|pipe| tokio::spawn(capture(pipe, stop.clone())));

    let ending = tokio::select! {
        status = child.wait() => Ending::Exited(status),
        () = tokio::time::sleep(Duration::from_millis(timeout_ms)) => Ending::TimedOut(timeout_ms),
        () = cancel.cancelled() => Ending::Cancelled,
    };

    // Also after a normal exit: a leftover background job would otherwise hold the pipes open
    // and outlive the call unsupervised.
    let killed = match pgid {
        Some(pgid) => kill_group(pgid).await,
        None => false,
    };
    let mut interrupted = true;
    let mut notes = Vec::new();
    match &ending {
        Ending::Exited(Ok(status)) => {
            interrupted = false;
            if !status.success() {
                notes.push(match status.code() {
                    Some(code) => format!("[exit code {code}]"),
                    None => "[killed by a signal]".to_owned(),
                });
            }
        }
        Ending::Exited(Err(error)) => notes.push(format!("[couldn't wait for the shell: {error}]")),
        Ending::TimedOut(ms) => notes.push(format!(
            "[Timed out after {ms} ms, so the process group was killed]"
        )),
        Ending::Cancelled => notes.push("[Cancelled, so the process group was killed]".to_owned()),
    }
    if !matches!(ending, Ending::Exited(_)) {
        if !killed {
            tracing::error!("couldn't kill a Bash process group");
            notes.push("[Couldn't kill the process group: processes may still run]".to_owned());
            // Better the shell alone than none: the wait below needs it gone.
            let _ = child.start_kill();
        }
        // Reaps the shell. An error here only means it was already gone.
        let _ = child.wait().await;
    }

    let timer = stop.clone();
    tokio::spawn(async move {
        tokio::time::sleep(DRAIN_GRACE).await;
        timer.cancel();
    });
    let collect = async |task: Option<tokio::task::JoinHandle<Captured>>| match task {
        Some(task) => task.await.unwrap_or_default(),
        None => Captured::default(),
    };
    let (stdout, stderr) = tokio::join!(collect(stdout), collect(stderr));

    let (stdout, stderr) = (stdout.text(), stderr.text());
    let is_error = interrupted || !matches!(&ending, Ending::Exited(Ok(s)) if s.success());
    let content: Vec<&str> = [stdout.as_str(), stderr.as_str()]
        .into_iter()
        .filter(|text| !text.is_empty())
        .chain(notes.iter().map(String::as_str))
        .collect();
    let content = if content.is_empty() {
        "(no output)".to_owned()
    } else {
        content.join("\n")
    };
    let content = if ctx.filter_output {
        filtered(ctx, content)
    } else {
        content
    };
    Ok(ToolOutput {
        content,
        is_error,
        structured: json!({
            "stdout": stdout,
            "stderr": stderr,
            "interrupted": interrupted,
            "isImage": false,
        }),
    })
}
