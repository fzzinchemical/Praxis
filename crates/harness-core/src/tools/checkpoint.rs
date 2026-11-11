//! Per-turn copies of files before `Write` or `Edit` changes them, for `/undo` (SPEC §8).

use super::{ToolContext, ToolError};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::PoisonError;

/// Shown with every undo: only file tools are checkpointed.
pub const UNDO_NOTE: &str = "Only Write and Edit changes are undone; Bash effects are not.";

#[derive(Debug, Default)]
pub(super) struct Checkpoints {
    /// `None` until a session gives us somewhere private to keep copies.
    dir: Option<PathBuf>,
    /// Per turn: each changed file and its saved copy, or `None` if the turn created it.
    turns: Vec<Vec<(PathBuf, Option<PathBuf>)>>,
    saved: u64,
}

impl ToolContext {
    /// Keeps originals under `dir` (created `0700`) from now on.
    pub fn checkpoint_into(&self, dir: PathBuf) {
        self.checkpoints().dir = Some(dir);
    }

    /// Starts a new undo step. Turns that changed nothing don't count as one.
    pub fn begin_turn(&self) {
        let mut cp = self.checkpoints();
        if cp.turns.last().is_none_or(|turn| !turn.is_empty()) {
            cp.turns.push(Vec::new());
        }
    }

    /// Saves `path` before its first change this turn. Without a directory it does nothing.
    pub(super) fn checkpoint(&self, path: &Path) -> Result<(), ToolError> {
        let mut cp = self.checkpoints();
        let Some(dir) = cp.dir.clone() else {
            return Ok(());
        };
        if cp.turns.is_empty() {
            cp.turns.push(Vec::new());
        }
        if cp
            .turns
            .last()
            .is_some_and(|t| t.iter().any(|(p, _)| p == path))
        {
            return Ok(());
        }
        let copy = dir.join((cp.saved + 1).to_string());
        let saved = (|| {
            let mut original = match File::open(path) {
                Ok(file) => file,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e),
            };
            DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&copy)?;
            io::copy(&mut original, &mut file)?;
            Ok(Some(copy))
        })()
        .map_err(ToolError::io("checkpoint", path))?;
        cp.saved += 1;
        let copy = saved;
        if let Some(turn) = cp.turns.last_mut() {
            turn.push((path.to_owned(), copy));
        }
        Ok(())
    }

    /// Restores the files the last changing turn wrote and returns their paths, newest first.
    pub fn undo(&self) -> io::Result<Vec<PathBuf>> {
        let turn = {
            let mut cp = self.checkpoints();
            while cp.turns.last().is_some_and(Vec::is_empty) {
                cp.turns.pop();
            }
            cp.turns.pop().unwrap_or_default()
        };
        let mut restored = Vec::new();
        for (path, copy) in turn.into_iter().rev() {
            match &copy {
                Some(copy) => super::files::replace_with(&path, |file| {
                    io::copy(&mut File::open(copy)?, file).map(drop)
                })?,
                None => match fs::remove_file(&path) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                    _ => {}
                },
            }
            // The model must read the file again before editing it.
            self.forget(&path);
            restored.push(path);
        }
        Ok(restored)
    }

    fn checkpoints(&self) -> std::sync::MutexGuard<'_, Checkpoints> {
        self.checkpoints
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::super::files::{edit, read, write};
    use super::*;
    use serde_json::{from_value, json};

    #[test]
    fn undo_restores_edited_files_and_removes_created_ones_from_the_last_turn() {
        let root = std::env::temp_dir().join(format!("harness-undo-{}", std::process::id()));
        let workspace = root.join("ws");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("old.txt"), "one\n").unwrap();
        let ctx = ToolContext::new(&workspace).unwrap();
        ctx.checkpoint_into(root.join("checkpoints"));
        let path = |name: &str| workspace.join(name).display().to_string();

        ctx.begin_turn();
        read(
            from_value(json!({"file_path": path("old.txt")})).unwrap(),
            &ctx,
        )
        .unwrap();
        let change = |old: &str, new: &str| {
            let input = json!({"file_path": path("old.txt"), "old_string": old, "new_string": new});
            edit(from_value(input).unwrap(), &ctx).unwrap();
        };
        change("one", "two");
        ctx.begin_turn();
        change("two", "three");
        change("three", "four");
        let input = json!({"file_path": path("sub/new.txt"), "content": "hi"});
        write(from_value(input).unwrap(), &ctx).unwrap();
        ctx.begin_turn();

        assert_eq!(ctx.undo().unwrap().len(), 2);
        assert_eq!(
            fs::read_to_string(workspace.join("old.txt")).unwrap(),
            "two\n"
        );
        assert!(!workspace.join("sub/new.txt").exists());
        ctx.undo().unwrap();
        assert_eq!(
            fs::read_to_string(workspace.join("old.txt")).unwrap(),
            "one\n"
        );
        assert!(ctx.undo().unwrap().is_empty());
        assert!(UNDO_NOTE.contains("Bash effects are not"));
        fs::remove_dir_all(&root).unwrap();
    }
}
