//! Append-only session logs, one JSON message per line (SPEC §8).

use crate::provider::Message;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// An open `<dir>/<id>.jsonl`. Logs hold source code and tool output, so only the owner can read them.
pub struct SessionLog {
    id: String,
    file: File,
}

impl SessionLog {
    /// Starts a new log under `dir`, named from the time and process id.
    pub fn create(dir: &Path) -> io::Result<Self> {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let id = format!("{secs}-{}", std::process::id());
        let file = open(dir, &id, true)?;
        Ok(Self { id, file })
    }

    /// Opens an existing log and returns its messages. A partial last line, left by a crash, is
    /// dropped from the file too, so the next record starts on a line of its own.
    pub fn resume(dir: &Path, id: &str) -> io::Result<(Self, Vec<Message>)> {
        let file = open(dir, id, false)?;
        let text = fs::read_to_string(path(dir, id)?)?;
        let complete = text.rfind('\n').map_or(0, |i| i + 1);
        let mut messages = Vec::new();
        for line in text[..complete].lines() {
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
    pub fn append(&mut self, message: &Message) -> io::Result<()> {
        let mut line = serde_json::to_string(message).map_err(io::Error::other)?;
        line.push('\n');
        self.file.write_all(line.as_bytes())
    }
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
