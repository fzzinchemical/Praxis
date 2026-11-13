//! Append-only session logs, one JSON message per line after a header line (SPEC §8).

use crate::provider::Message;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// An open `<dir>/<id>.jsonl`. Logs hold source code and tool output, so only the owner can read them.
pub struct SessionLog {
    id: String,
    file: File,
}

/// The first line of every new log: where the session ran and when it started (UTC, Unix seconds).
#[derive(Serialize, Deserialize)]
struct HeaderLine {
    session: Header,
}

#[derive(Serialize, Deserialize)]
struct Header {
    workspace: String,
    started: u64,
}

/// One entry in the session index. Last activity is the log's mtime, since logs only grow.
#[derive(Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    pub started: u64,
    pub last_active: u64,
    /// The first user prompt, trimmed; empty if the log has none yet.
    pub title: String,
}

impl SessionLog {
    /// Starts a new log under `dir` for a session in `workspace`, named from the time and process id.
    pub fn create(dir: &Path, workspace: &Path) -> io::Result<Self> {
        let now = unix_time(SystemTime::now());
        let id = format!("{}-{}", now.as_nanos(), std::process::id());
        let mut file = open(dir, &id, true)?;
        lock(&file, &id)?;
        let header = HeaderLine {
            session: Header {
                workspace: canonical(workspace)?,
                started: now.as_secs(),
            },
        };
        let mut line = serde_json::to_string(&header).map_err(io::Error::other)?;
        line.push('\n');
        file.write_all(line.as_bytes())?;
        Ok(Self { id, file })
    }

    /// Opens an existing log and returns its messages. A partial last line, left by a crash, is
    /// dropped from the file too, so the next record starts on a line of its own.
    pub fn resume(dir: &Path, id: &str) -> io::Result<(Self, Vec<Message>)> {
        let file = open(dir, id, false)?;
        lock(&file, id)?;
        let text = fs::read_to_string(path(dir, id)?)?;
        let complete = text.rfind('\n').map_or(0, |i| i + 1);
        let mut messages = Vec::new();
        for (i, line) in text[..complete].lines().enumerate() {
            // Logs from before the header existed have none, so only the first line can be one.
            if i == 0 && serde_json::from_str::<HeaderLine>(line).is_ok() {
                continue;
            }
            messages.push(serde_json::from_str(line).map_err(io::Error::other)?);
        }
        if complete < text.len() {
            file.set_len(complete as u64)?;
        }
        let id = id.to_owned();
        Ok((Self { id, file }, messages))
    }

    /// Wraps a file opened elsewhere, such as `/dev/full` to test write failures.
    pub fn from_file(id: String, file: File) -> Self {
        Self { id, file }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Writes one record as a single line, so a crash can only cut the last one short.
    /// shortcut: logs are never deleted or trimmed, so disk use grows with every turn. Upgrade with a per-user size or age cap.
    pub fn append(&mut self, message: &Message) -> io::Result<()> {
        let mut line = serde_json::to_string(message).map_err(io::Error::other)?;
        line.push('\n');
        self.file.write_all(line.as_bytes())
    }
}

/// Lists the sessions logged for `workspace`, newest activity first. It reads each log's header,
/// mtime and first user line only. A log without a readable header belongs to no workspace.
pub fn list(dir: &Path, workspace: &Path) -> io::Result<Vec<SessionInfo>> {
    let wanted = canonical(workspace)?;
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut sessions: Vec<SessionInfo> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let id = name.strip_suffix(".jsonl")?.to_owned();
            // shortcut: a log that fails to read is skipped, not reported, so one bad file can't hide the rest.
            info(dir, &id, &wanted).ok().flatten()
        })
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.last_active));
    Ok(sessions)
}

/// `None` if the log's header names another workspace or is missing.
fn info(dir: &Path, id: &str, workspace: &str) -> io::Result<Option<SessionInfo>> {
    let file = File::open(path(dir, id)?)?;
    let last_active = unix_time(file.metadata()?.modified()?).as_secs();
    let mut lines = BufReader::new(file).lines();
    let Some(Ok(first)) = lines.next() else {
        return Ok(None);
    };
    let Ok(HeaderLine { session }) = serde_json::from_str(&first) else {
        return Ok(None);
    };
    if session.workspace != workspace {
        return Ok(None);
    }
    let title = lines
        .map_while(Result::ok)
        .find_map(|line| match serde_json::from_str(&line) {
            Ok(Message::User { content }) => Some(content),
            _ => None,
        })
        .unwrap_or_default();
    Ok(Some(SessionInfo {
        id: id.to_owned(),
        started: session.started,
        last_active,
        title: title.trim().to_owned(),
    }))
}

/// The workspace as the header stores it. shortcut: a non-UTF-8 path is stored lossily, so it can
/// match another workspace whose name differs only in those bytes.
fn canonical(workspace: &Path) -> io::Result<String> {
    Ok(fs::canonicalize(workspace)?.to_string_lossy().into_owned())
}

fn unix_time(time: SystemTime) -> Duration {
    time.duration_since(UNIX_EPOCH).unwrap_or_default()
}

/// Ids come from the command line, so they must not name a path elsewhere.
fn path(dir: &Path, id: &str) -> io::Result<PathBuf> {
    let valid = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid session id `{id}`"),
        ));
    }
    Ok(dir.join(format!("{id}.jsonl")))
}

/// Holds an exclusive lock on the log for as long as its `File` lives, so two processes can't
/// append to one session or race for the same checkpoint number.
fn lock(file: &File, id: &str) -> io::Result<()> {
    file.try_lock().map_err(|e| match e {
        TryLockError::WouldBlock => io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("session `{id}` is open in another window"),
        ),
        TryLockError::Error(e) => e,
    })
}

fn open(dir: &Path, id: &str, new: bool) -> io::Result<File> {
    let path = path(dir, id)?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    OpenOptions::new()
        .append(true)
        .create_new(new)
        .mode(0o600)
        .open(path)
}
