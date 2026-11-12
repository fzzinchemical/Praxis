//! Claude Code's `settings.json` (SPEC §8, §12). Only `permissions.allow`, `permissions.deny`
//! and `hooks` are read; every other key belongs to Claude Code and is ignored.

use serde::Deserialize;
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub permissions: Permissions,
    /// The event map, as in a plugin's hooks file (SPEC §12).
    pub hooks: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Permissions {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

/// `Ok(None)` for a missing file. Errors carry the parser's message only; the caller adds the
/// path.
pub fn read(path: &Path) -> Result<Option<Settings>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| e.to_string())
}
