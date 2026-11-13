//! Workspace confinement (SPEC §9) and hard denials (SPEC §10).

use super::ToolError;
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};

// shortcut: only the workspace is permitted, so outside paths are refused rather than asked about;
// add per-path approval and extra_read/extra_write with the permissions and config tasks.
// shortcut: a path swapped for a symlink between this check and the file operation isn't caught;
// use openat2 if tools ever run beside untrusted local processes.
/// Resolves `raw` against the real workspace path, following symlinks on every existing part.
/// A dangling symlink or a `..` below a missing directory is refused: neither can be resolved,
/// and creating through them could land outside.
pub fn resolve(workspace: &Path, raw: &str) -> Result<PathBuf, ToolError> {
    let full = workspace.join(raw);
    let mut missing = Vec::new();
    let mut current = full.as_path();
    let base = loop {
        match fs::symlink_metadata(current) {
            Ok(_) => {
                break fs::canonicalize(current).map_err(ToolError::io("resolve", current))?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let (Some(name), Some(parent)) = (current.file_name(), current.parent()) else {
                    return Err(ToolError::Refused(format!(
                        "{} can't be resolved. Use a path without `..` through missing folders.",
                        full.display()
                    )));
                };
                missing.push(name);
                current = parent;
            }
            Err(e) => return Err(ToolError::io("resolve", current)(e)),
        }
    };
    if !base.starts_with(workspace) {
        return Err(ToolError::OutsideWorkspace(full.display().to_string()));
    }
    Ok(missing
        .iter()
        .rev()
        .fold(base, |path, name| path.join(name)))
}

/// Refuses paths whose change could plant code or widen permissions, at any depth so nested
/// repositories count. `path` must come from [`resolve`]. User config (also hard-denied) lives
/// outside the workspace, so [`resolve`] already refuses it.
pub fn check_writable(workspace: &Path, path: &Path) -> Result<(), ToolError> {
    let names: Vec<&OsStr> = path
        .strip_prefix(workspace)
        .unwrap_or(path)
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    // Any `.git` component: submodules keep hooks and config under .git/modules/<name>, and a
    // worktree's `.git` file points at a git dir that can be anywhere in the workspace.
    let denied = names
        .iter()
        .any(|n| *n == ".praxis" || *n == ".mcp.json" || *n == ".git");
    if denied {
        return Err(ToolError::HardDenied(path.display().to_string()));
    }
    Ok(())
}
