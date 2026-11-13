//! Approvals of project config that widens permissions (SPEC §8). Each project's approval is
//! the SHA-256 of exactly what was approved, so any change needs approving again.

use crate::tools::files::replace_with;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, DirBuilder};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

/// `~/.local/share/praxis/trust.json`: project config path to approved hash.
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

    /// Holds an exclusive lock on a sibling file across the read-modify-write, so concurrent
    /// sets from other sessions cannot drop each other's keys.
    pub fn set(&self, key: &str, value: &str) -> Result<(), String> {
        let fail = |e: std::io::Error| format!("{}: {e}", self.path.display());
        if let Some(dir) = self.path.parent() {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(fail)?;
        }
        let lock_path = self.path.with_extension("json.lock");
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&lock_path)
            .map_err(fail)?;
        lock.lock().map_err(fail)?;
        let mut approvals = self.read()?;
        approvals.insert(key.to_owned(), value.to_owned());
        let text = serde_json::to_string_pretty(&approvals).map_err(|e| e.to_string())?;
        replace_with(&self.path, |file| {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.write_all(text.as_bytes())
        })
        .map_err(fail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_sets_leave_the_store_valid_json() {
        let dir =
            std::env::temp_dir().join(format!("praxis-trust-{}-concurrent", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("trust.json");
        let store = TrustStore::new(path.clone());
        std::thread::scope(|scope| {
            for thread in 0..16 {
                let store = store.clone();
                scope.spawn(move || {
                    for n in 0..20 {
                        store.set(&format!("project-{thread}-{n}"), "hash").unwrap();
                    }
                });
            }
        });
        let text = fs::read_to_string(&path).unwrap();
        let stored = serde_json::from_str::<BTreeMap<String, String>>(&text).unwrap();
        assert_eq!(stored.len(), 16 * 20, "concurrent sets lost keys");
        fs::remove_dir_all(&dir).unwrap();
    }
}
