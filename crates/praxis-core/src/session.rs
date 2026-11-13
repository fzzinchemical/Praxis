//! Append-only session logs, one JSON message per line after a header line (SPEC §8).

use crate::provider::Message;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Session id -> folder name, beside the logs. A side file keeps the logs themselves unchanged.
const FOLDERS_FILE: &str = "folders.json";
const FOLDER_NAME_MAX: usize = 64;

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
    /// The folder the session was moved to, if any.
    pub folder: Option<String>,
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

    /// Opens an existing log and returns its main messages, without subagent entries.
    pub fn resume(dir: &Path, id: &str) -> io::Result<(Self, Vec<Message>)> {
        let (log, entries) = Self::resume_entries(dir, id)?;
        let messages = entries
            .into_iter()
            .filter(|e| e.subagent_of.is_none())
            .map(|e| e.message)
            .collect();
        Ok((log, messages))
    }

    /// Opens an existing log and returns every entry in order, subagent ones included. A partial
    /// last line, left by a crash, is dropped from the file too, so the next record starts on a
    /// line of its own.
    pub fn resume_entries(dir: &Path, id: &str) -> io::Result<(Self, Vec<Entry>)> {
        let file = open(dir, id, false)?;
        lock(&file, id)?;
        let text = fs::read_to_string(path(dir, id)?)?;
        let complete = text.rfind('\n').map_or(0, |i| i + 1);
        let mut entries = Vec::new();
        for (i, line) in text[..complete].lines().enumerate() {
            // Logs from before the header existed have none, so only the first line can be one.
            if i == 0 && serde_json::from_str::<HeaderLine>(line).is_ok() {
                continue;
            }
            if let Ok(RewindLine { rewind }) = serde_json::from_str(line) {
                cut_turn(&mut entries, rewind.turn);
                continue;
            }
            entries.push(serde_json::from_str(line).map_err(io::Error::other)?);
        }
        if complete < text.len() {
            file.set_len(complete as u64)?;
        }
        let id = id.to_owned();
        Ok((Self { id, file }, entries))
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
    /// shortcut: an ended session's checkpoint copies are never deleted (only undo and turn expiry remove them). Upgrade by sweeping the checkpoint folder of sessions that no longer exist.
    pub fn append(&mut self, message: &Message) -> io::Result<()> {
        self.write_line(&EntryRef {
            subagent_of: None,
            message,
        })
    }

    /// Writes a message a subagent produced under the `Agent` call `parent`. Resuming keeps it
    /// out of the main history, and replay nests it under that call.
    pub fn append_subagent(&mut self, parent: &str, message: &Message) -> io::Result<()> {
        self.write_line(&EntryRef {
            subagent_of: Some(parent),
            message,
        })
    }

    /// Records that the chat was rewound to before user turn `turn` (1-based). Replay drops that
    /// turn and every later one, so a resumed chat shows the shortened history.
    pub fn append_rewind(&mut self, turn: usize) -> io::Result<()> {
        self.write_line(&RewindLine {
            rewind: Rewind { turn },
        })
    }

    fn write_line(&mut self, entry: &impl Serialize) -> io::Result<()> {
        let mut line = serde_json::to_string(entry).map_err(io::Error::other)?;
        line.push('\n');
        self.file.write_all(line.as_bytes())
    }
}

/// One logged message. Lines from before subagent entries existed have no `subagent_of`.
#[derive(Debug, Deserialize)]
pub struct Entry {
    /// The `Agent` tool call whose subagent produced this message, if any.
    #[serde(default)]
    pub subagent_of: Option<String>,
    #[serde(flatten)]
    pub message: Message,
}

/// A rewind record. Logs from before rewinds existed have none, so old logs replay unchanged.
#[derive(Serialize, Deserialize)]
struct RewindLine {
    rewind: Rewind,
}

#[derive(Serialize, Deserialize)]
struct Rewind {
    turn: usize,
}

/// Drops main user turn `turn` (1-based) and everything after it, subagent entries included.
fn cut_turn(entries: &mut Vec<Entry>, turn: usize) {
    let is_turn_start =
        |e: &Entry| e.subagent_of.is_none() && matches!(e.message, Message::User { .. });
    let start = turn.checked_sub(1).and_then(|n| {
        entries
            .iter()
            .enumerate()
            .filter(|(_, e)| is_turn_start(e))
            .nth(n)
    });
    if let Some((at, _)) = start {
        entries.truncate(at);
    }
}

/// The borrowed form of [`Entry`] written to the log, leaving the field out when it is unset.
#[derive(Serialize)]
struct EntryRef<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    subagent_of: Option<&'a str>,
    #[serde(flatten)]
    message: &'a Message,
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
    let folders = read_folders(dir);
    let mut sessions: Vec<SessionInfo> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let id = name.strip_suffix(".jsonl")?.to_owned();
            // shortcut: a log that fails to read is skipped, not reported, so one bad file can't hide the rest.
            info(dir, &id, &wanted).ok().flatten()
        })
        .map(|s| SessionInfo {
            folder: folders.get(&s.id).cloned(),
            ..s
        })
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.last_active));
    Ok(sessions)
}

/// Removes one session's log and its folder entry. The lock keeps it from going while another window still writes to it.
pub fn delete(dir: &Path, id: &str) -> io::Result<()> {
    let file = open(dir, id, false)?;
    lock(&file, id)?;
    fs::remove_file(path(dir, id)?)?;
    let mut folders = read_folders(dir);
    if folders.remove(id).is_some() {
        write_folders(dir, &folders)?;
    }
    Ok(())
}

/// Moves a session into the folder `name`. An empty name (after trimming) takes it out of its folder.
/// Names are trimmed, at most 64 characters, and free of control characters.
pub fn set_folder(dir: &Path, id: &str, name: &str) -> io::Result<()> {
    if !path(dir, id)?.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no session `{id}`"),
        ));
    }
    let name = name.trim();
    let mut folders = read_folders(dir);
    if name.is_empty() {
        folders.remove(id);
    } else {
        if name.chars().count() > FOLDER_NAME_MAX || name.chars().any(char::is_control) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "folder names are at most {FOLDER_NAME_MAX} characters with no control characters"
                ),
            ));
        }
        folders.insert(id.to_owned(), name.to_owned());
    }
    write_folders(dir, &folders)
}

/// Every session's folder. A missing or unreadable file reads as no folders, so sessions still list.
/// shortcut: the next folder change rewrites a corrupt file from scratch, so its old entries are lost. Upgrade by keeping the bad file aside.
fn read_folders(dir: &Path) -> BTreeMap<String, String> {
    fs::read_to_string(dir.join(FOLDERS_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Writes the folder file through a temp file and a rename, so a crash never leaves it half-written.
fn write_folders(dir: &Path, folders: &BTreeMap<String, String>) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let text = serde_json::to_string(folders).map_err(io::Error::other)?;
    let temp = dir.join(format!("{FOLDERS_FILE}.{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    fs::rename(temp, dir.join(FOLDERS_FILE))
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
        folder: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Message {
        Message::User {
            content: text.to_owned(),
        }
    }

    fn reply(text: &str) -> Message {
        Message::Assistant {
            content: Some(text.to_owned()),
            tool_calls: Vec::new(),
        }
    }

    fn texts(messages: &[Message]) -> Vec<&str> {
        messages
            .iter()
            .map(|m| match m {
                Message::User { content } => content.as_str(),
                Message::Assistant { content, .. } => content.as_deref().unwrap_or_default(),
                _ => "?",
            })
            .collect()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("praxis-session-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("ws")).unwrap();
        dir
    }

    #[test]
    fn replay_after_a_rewind_drops_the_rewound_turns_and_their_subagent_entries() {
        let dir = scratch("rewind");
        let mut log = SessionLog::create(&dir, &dir.join("ws")).unwrap();
        let id = log.id().to_owned();
        log.append(&user("one")).unwrap();
        log.append(&reply("a")).unwrap();
        log.append(&user("two")).unwrap();
        log.append_subagent("call", &reply("from subagent"))
            .unwrap();
        log.append(&reply("b")).unwrap();
        log.append_rewind(2).unwrap();
        log.append(&user("three")).unwrap();
        drop(log);

        let (_log, messages) = SessionLog::resume(&dir, &id).unwrap();

        assert_eq!(texts(&messages), ["one", "a", "three"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_log_without_rewind_entries_replays_every_message() {
        let dir = scratch("no-rewind");
        let mut log = SessionLog::create(&dir, &dir.join("ws")).unwrap();
        let id = log.id().to_owned();
        log.append(&user("one")).unwrap();
        log.append(&reply("a")).unwrap();
        drop(log);

        let (_log, messages) = SessionLog::resume(&dir, &id).unwrap();

        assert_eq!(texts(&messages), ["one", "a"]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
