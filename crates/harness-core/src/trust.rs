//! Approvals of project config that widens permissions (SPEC §8). Each project's approval is
//! the SHA-256 of exactly what was approved, so any change needs approving again.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

/// `~/.local/share/harness/trust.json`: project config path to approved hash.
#[derive(Debug, Clone)]
pub struct TrustStore {
    path: PathBuf,
}

pub fn hash(content: &str) -> String {
    Sha256::digest(content.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl TrustStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// A missing store approves nothing. A broken one is an error, never a silent reset.
    fn read(&self) -> Result<BTreeMap<String, String>, String> {
        match fs::read_to_string(&self.path) {
            Ok(text) => {
                serde_json::from_str(&text).map_err(|e| format!("{}: {e}", self.path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(format!("{}: {e}", self.path.display())),
        }
    }

    pub fn is_approved(&self, project: &Path, hash: &str) -> Result<bool, String> {
        Ok(self.get(&project.display().to_string())? == Some(hash.to_owned()))
    }

    pub fn approve(&self, project: &Path, hash: &str) -> Result<(), String> {
        self.set(&project.display().to_string(), hash)
    }

    /// Any stored value; project paths and MCP tool pins share the store.
    pub fn get(&self, key: &str) -> Result<Option<String>, String> {
        Ok(self.read()?.remove(key))
    }

    pub fn set(&self, key: &str, value: &str) -> Result<(), String> {
        let mut approvals = self.read()?;
        approvals.insert(key.to_owned(), value.to_owned());
        let text = serde_json::to_string_pretty(&approvals).map_err(|e| e.to_string())?;
        let fail = |e: std::io::Error| format!("{}: {e}", self.path.display());
        if let Some(dir) = self.path.parent() {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(fail)?;
        }
        // Written beside the store and renamed, so a crash never leaves half a file.
        let partial = self.path.with_extension("json.partial");
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&partial)
            .and_then(|mut file| file.write_all(text.as_bytes()))
            .and_then(|()| fs::rename(&partial, &self.path))
            .map_err(fail)
    }
}
