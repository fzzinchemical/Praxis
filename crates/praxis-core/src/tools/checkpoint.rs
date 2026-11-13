//! Per-turn copies of files before `Write` or `Edit` changes them, for `/undo` (SPEC §8).

use super::{ToolContext, ToolError};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::PoisonError;

/// Shown with every undo: only file tools are checkpointed.
/// Undo steps whose copies are kept; older turns are dropped with their copy files.
/// shortcut: copies are kept for the last 20 turns only, so `/undo` cannot go further back. Upgrade if users need deeper undo.
const CHECKPOINT_TURNS: usize = 20;

pub const UNDO_NOTE: &str = "Only Write and Edit changes are undone; Bash effects are not.";

/// One chat turn's undo step: each file it changed, with its saved copy or `None` if the turn created it.
#[derive(Debug)]
struct Step {
    turn: usize,
    files: Vec<(PathBuf, Option<PathBuf>)>,
}

#[derive(Debug, Default)]
pub(super) struct Checkpoints {
    /// `None` until a session gives us somewhere private to keep copies.
    dir: Option<PathBuf>,
    turns: Vec<Step>,
    saved: u64,
    /// The newest turn whose copies the window deleted. That turn and older ones can't be rewound.
    expired_through: usize,
}

impl ToolContext {
    /// Keeps originals under `dir` (created `0700`) from now on.
    /// A resumed session reuses the folder, so numbering starts above the copies already there.
    /// Copies from earlier runs are deleted: their undo steps were in memory and are gone.
    pub fn checkpoint_into(&self, dir: PathBuf) {
        let mut cp = self.checkpoints();
        let live: Vec<PathBuf> = cp
            .turns
            .iter()
            .flat_map(|step| &step.files)
            .filter_map(|(_, c)| c.clone())
            .collect();
        let mut highest = 0;
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let Some(number) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u64>().ok())
            else {
                continue;
            };
            highest = highest.max(number);
            let path = entry.path();
            if live.contains(&path) {
                continue;
            }
            if let Err(e) = fs::remove_file(&path) {
                tracing::warn!(copy = %path.display(), error = %e, "could not delete an old checkpoint");
            }
        }
        cp.dir = Some(dir);
        cp.saved = highest;
    }

    /// Starts undo step `turn`, the 1-based number of the chat turn. Turns that changed nothing
    /// don't count as a step. Steps beyond `CHECKPOINT_TURNS` are dropped, and their copies deleted.
    pub fn begin_turn(&self, turn: usize) {
        let mut guard = self.checkpoints();
        let cp = &mut *guard;
        match cp.turns.last_mut() {
            Some(step) if step.files.is_empty() => step.turn = turn,
            _ => cp.turns.push(Step {
                turn,
                files: Vec::new(),
            }),
        }
        let expired = cp.turns.len().saturating_sub(CHECKPOINT_TURNS);
        for step in cp.turns.drain(..expired) {
            cp.expired_through = step.turn;
            for (_, copy) in step.files {
                let Some(copy) = copy else { continue };
                match fs::remove_file(&copy) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => {
                        tracing::warn!(copy = %copy.display(), error = %e, "could not delete an old checkpoint");
                    }
                    _ => {}
                }
            }
        }
    }

    /// Saves `path` before its first change this turn. Without a directory it does nothing.
    pub(super) fn checkpoint(&self, path: &Path) -> Result<(), ToolError> {
        let mut cp = self.checkpoints();
        let Some(dir) = cp.dir.clone() else {
            return Ok(());
        };
        if cp.turns.is_empty() {
            cp.turns.push(Step {
                turn: 0,
                files: Vec::new(),
            });
        }
        if cp
            .turns
            .last()
            .is_some_and(|step| step.files.iter().any(|(p, _)| p == path))
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
        if let Some(step) = cp.turns.last_mut() {
            step.files.push((path.to_owned(), copy));
        }
        Ok(())
    }

    /// Restores the files the last changing turn wrote and returns their paths, newest first.
    /// Every file is attempted; failures are kept for a later undo and all named in the error.
    pub fn undo(&self) -> io::Result<Vec<PathBuf>> {
        let steps = {
            let mut cp = self.checkpoints();
            let Some(at) = cp.turns.iter().rposition(|step| !step.files.is_empty()) else {
                return Ok(Vec::new());
            };
            cp.turns.split_off(at)
        };
        self.restore_steps(steps)
    }

    /// Restores the files that chat turn `turn` and every later one changed, newest first, and drops
    /// their undo steps. Fails for a turn whose copies the window already deleted.
    pub fn rewind_to(&self, turn: usize) -> io::Result<Vec<PathBuf>> {
        let steps = {
            let mut cp = self.checkpoints();
            if cp.dir.is_none() {
                return Err(io::Error::other("file checkpoints are off"));
            }
            if turn <= cp.expired_through {
                return Err(io::Error::other(format!(
                    "turn {turn} is older than the last {CHECKPOINT_TURNS} turns"
                )));
            }
            let at = cp.turns.partition_point(|step| step.turn < turn);
            cp.turns.split_off(at)
        };
        self.restore_steps(steps)
    }

    /// Restores every file of `steps`, newest first. Failures stay in the undo history for a later try.
    fn restore_steps(&self, steps: Vec<Step>) -> io::Result<Vec<PathBuf>> {
        let mut restored = Vec::new();
        let mut failed = Vec::new();
        let mut failures = Vec::new();
        for step in steps.into_iter().rev() {
            let mut kept = Vec::new();
            for (path, copy) in step.files.into_iter().rev() {
                match restore(&path, copy.as_deref()) {
                    Ok(()) => {
                        // The model must read the file again before editing it.
                        self.forget(&path);
                        if let Some(copy) = &copy {
                            // The restore already succeeded, so a leftover copy only wastes disk.
                            if let Err(e) = fs::remove_file(copy) {
                                tracing::warn!(copy = %copy.display(), error = %e, "could not delete a restored checkpoint");
                            }
                        }
                        restored.push(path);
                    }
                    Err(e) => {
                        failures.push(format!("{}: {e}", path.display()));
                        kept.push((path, copy));
                    }
                }
            }
            if !kept.is_empty() {
                kept.reverse();
                failed.push(Step {
                    turn: step.turn,
                    files: kept,
                });
            }
        }
        if failures.is_empty() {
            return Ok(restored);
        }
        failed.reverse();
        self.checkpoints().turns.extend(failed);
        // Users need to know which files already changed back, not only which ones failed.
        let done = if restored.is_empty() {
            String::new()
        } else {
            let names: Vec<_> = restored.iter().map(|p| p.display().to_string()).collect();
            format!("Restored {}. ", names.join(", "))
        };
        Err(io::Error::other(format!(
            "{done}Can't restore {}",
            failures.join("; ")
        )))
    }

    fn checkpoints(&self) -> std::sync::MutexGuard<'_, Checkpoints> {
        self.checkpoints
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// Puts `path` back as `copy` was, or removes it when the turn created it.
fn restore(path: &Path, copy: Option<&Path>) -> io::Result<()> {
    match copy {
        Some(copy) => super::files::replace_with(path, |file| {
            io::copy(&mut File::open(copy)?, file).map(drop)
        }),
        None => match fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::super::files::{edit, read, write};
    use super::*;
    use serde_json::{from_value, json};

    #[test]
    fn undo_restores_edited_files_and_removes_created_ones_from_the_last_turn() {
        let root = std::env::temp_dir().join(format!("praxis-undo-{}", std::process::id()));
        let workspace = root.join("ws");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("old.txt"), "one\n").unwrap();
        let ctx = ToolContext::new(&workspace).unwrap();
        ctx.checkpoint_into(root.join("checkpoints"));
        let path = |name: &str| workspace.join(name).display().to_string();

        ctx.begin_turn(1);
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
        ctx.begin_turn(2);
        change("two", "three");
        change("three", "four");
        let input = json!({"file_path": path("sub/new.txt"), "content": "hi"});
        write(from_value(input).unwrap(), &ctx).unwrap();
        ctx.begin_turn(3);

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
        assert_eq!(fs::read_dir(root.join("checkpoints")).unwrap().count(), 0);
        assert!(UNDO_NOTE.contains("Bash effects are not"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn undo_restores_the_other_files_and_names_the_one_it_cannot_restore() {
        let root = std::env::temp_dir().join(format!("praxis-undo-partial-{}", std::process::id()));
        let workspace = root.join("ws");
        fs::create_dir_all(workspace.join("gone")).unwrap();
        fs::write(workspace.join("a.txt"), "one\n").unwrap();
        fs::write(workspace.join("gone/b.txt"), "one\n").unwrap();
        let ctx = ToolContext::new(&workspace).unwrap();
        ctx.checkpoint_into(root.join("checkpoints"));
        let path = |name: &str| workspace.join(name).display().to_string();

        ctx.begin_turn(1);
        for name in ["a.txt", "gone/b.txt"] {
            read(from_value(json!({"file_path": path(name)})).unwrap(), &ctx).unwrap();
            let input = json!({"file_path": path(name), "old_string": "one", "new_string": "two"});
            edit(from_value(input).unwrap(), &ctx).unwrap();
        }
        ctx.begin_turn(2);
        fs::remove_dir_all(workspace.join("gone")).unwrap();

        let err = ctx.undo().unwrap_err().to_string();
        assert!(err.contains("gone/b.txt"), "{err}");
        assert!(
            err.contains(&path("a.txt")),
            "restored file not named: {err}"
        );
        assert_eq!(
            fs::read_to_string(workspace.join("a.txt")).unwrap(),
            "one\n"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn resumed_session_edits_and_undoes_into_the_same_checkpoint_folder() {
        let root = std::env::temp_dir().join(format!("praxis-resume-{}", std::process::id()));
        let workspace = root.join("ws");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("a.txt"), "one\n").unwrap();
        let checkpoints = root.join("checkpoints");
        let path = |name: &str| workspace.join(name).display().to_string();
        let edit_input = |old: &str, new: &str| {
            from_value(json!({"file_path": path("a.txt"), "old_string": old, "new_string": new}))
                .unwrap()
        };

        let first = ToolContext::new(&workspace).unwrap();
        first.checkpoint_into(checkpoints.clone());
        first.begin_turn(1);
        read(
            from_value(json!({"file_path": path("a.txt")})).unwrap(),
            &first,
        )
        .unwrap();
        edit(edit_input("one", "two"), &first).unwrap();
        drop(first);

        let resumed = ToolContext::new(&workspace).unwrap();
        resumed.checkpoint_into(checkpoints);
        resumed.begin_turn(1);
        read(
            from_value(json!({"file_path": path("a.txt")})).unwrap(),
            &resumed,
        )
        .unwrap();
        edit(edit_input("two", "three"), &resumed).unwrap();
        assert_eq!(
            fs::read_to_string(workspace.join("a.txt")).unwrap(),
            "three\n"
        );

        assert_eq!(resumed.undo().unwrap().len(), 1);
        assert_eq!(
            fs::read_to_string(workspace.join("a.txt")).unwrap(),
            "two\n"
        );
        fs::remove_dir_all(&root).unwrap();
    }
    #[test]
    fn checkpoints_older_than_the_last_turns_are_deleted_and_undo_still_works() {
        let root = std::env::temp_dir().join(format!("praxis-retention-{}", std::process::id()));
        let workspace = root.join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let file = workspace.join("a.txt");
        fs::write(&file, "v0\n").unwrap();
        let checkpoints = root.join("checkpoints");
        let ctx = ToolContext::new(&workspace).unwrap();
        ctx.checkpoint_into(checkpoints.clone());
        let path = file.display().to_string();

        let turns = CHECKPOINT_TURNS + 1;
        for i in 0..turns {
            ctx.begin_turn(i + 1);
            read(from_value(json!({"file_path": path})).unwrap(), &ctx).unwrap();
            let input = json!({"file_path": path, "old_string": format!("v{i}\n"), "new_string": format!("v{}\n", i + 1)});
            edit(from_value(input).unwrap(), &ctx).unwrap();
        }

        let copies = fs::read_dir(&checkpoints).unwrap().count();
        assert_eq!(copies, CHECKPOINT_TURNS);
        assert!(
            !checkpoints.join("1").exists(),
            "oldest copy should be deleted"
        );

        assert_eq!(ctx.undo().unwrap().len(), 1);
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            format!("v{CHECKPOINT_TURNS}\n")
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn resumed_session_deletes_copies_from_earlier_runs_that_undo_cannot_reach() {
        let root = std::env::temp_dir().join(format!("praxis-prune-{}", std::process::id()));
        let workspace = root.join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let file = workspace.join("a.txt");
        fs::write(&file, "v0\n").unwrap();
        let checkpoints = root.join("checkpoints");
        fs::create_dir_all(&checkpoints).unwrap();
        // Copies left by an earlier run; its undo history died with that process.
        for n in 1..=25 {
            fs::write(checkpoints.join(n.to_string()), "old\n").unwrap();
        }
        let path = file.display().to_string();

        let resumed = ToolContext::new(&workspace).unwrap();
        resumed.checkpoint_into(checkpoints.clone());
        let numbered = |dir: &Path| {
            fs::read_dir(dir)
                .unwrap()
                .filter_map(|e| e.unwrap().file_name().into_string().ok())
                .filter(|name| name.parse::<u64>().is_ok())
                .count()
        };
        assert_eq!(numbered(&checkpoints), 0, "old copies must be deleted");

        resumed.begin_turn(1);
        read(from_value(json!({"file_path": path})).unwrap(), &resumed).unwrap();
        let input = json!({"file_path": path, "old_string": "v0\n", "new_string": "v1\n"});
        edit(from_value(input).unwrap(), &resumed).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "v1\n");
        assert_eq!(resumed.undo().unwrap().len(), 1);
        assert_eq!(fs::read_to_string(&file).unwrap(), "v0\n");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rewind_restores_files_from_turn_n_on_and_refuses_turns_past_the_window() {
        let root = std::env::temp_dir().join(format!("praxis-rewind-{}", std::process::id()));
        let workspace = root.join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let file = workspace.join("a.txt");
        fs::write(&file, "v0\n").unwrap();
        let ctx = ToolContext::new(&workspace).unwrap();
        ctx.checkpoint_into(root.join("checkpoints"));
        let path = file.display().to_string();

        let turns = CHECKPOINT_TURNS + 1;
        for i in 0..turns {
            ctx.begin_turn(i + 1);
            read(from_value(json!({"file_path": path})).unwrap(), &ctx).unwrap();
            let input = json!({"file_path": path, "old_string": format!("v{i}\n"), "new_string": format!("v{}\n", i + 1)});
            edit(from_value(input).unwrap(), &ctx).unwrap();
        }

        let err = ctx.rewind_to(1).unwrap_err().to_string();
        assert!(err.contains("older than the last"), "{err}");
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            format!("v{turns}\n"),
            "a refused rewind changes nothing"
        );

        // Turn 2 changed v1 to v2, so rewinding to it restores v1 and drops turns 2 onward.
        // Turns 2 to 21 each changed the file once, so twenty restores.
        assert_eq!(ctx.rewind_to(2).unwrap().len(), CHECKPOINT_TURNS);
        assert_eq!(fs::read_to_string(&file).unwrap(), "v1\n");
        assert!(ctx.rewind_to(2).unwrap().is_empty());
        assert!(ctx.rewind_to(1).is_err());
        fs::remove_dir_all(&root).unwrap();
    }
}
