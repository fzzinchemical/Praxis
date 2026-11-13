//! `Read`, `Write` and `Edit`.

use super::confine::{check_writable, resolve};
use super::{ToolContext, ToolError, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::hash_map::DefaultHasher;
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const DEFAULT_LINE_LIMIT: usize = 2000;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadInput {
    file_path: String,
    /// First line to return, counting from 1.
    offset: Option<usize>,
    /// How many lines to return. Defaults to 2000.
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteInput {
    file_path: String,
    content: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditInput {
    file_path: String,
    /// Exact text to replace, including whitespace.
    old_string: String,
    new_string: String,
    /// Replace every occurrence. Without it, `old_string` must be unique.
    replace_all: Option<bool>,
}

fn hash_of(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Reads a regular file only: a FIFO or device would hang or never end.
fn read_bytes(path: &Path) -> Result<Vec<u8>, ToolError> {
    let meta = fs::metadata(path).map_err(ToolError::io("read", path))?;
    if !meta.is_file() {
        return Err(ToolError::Refused(format!(
            "{} is not a regular file.",
            path.display()
        )));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(ToolError::Refused(format!(
            "{} is larger than {MAX_FILE_BYTES} bytes. Use Grep to find what you need.",
            path.display()
        )));
    }
    fs::read(path).map_err(ToolError::io("read", path))
}

fn text_of<'a>(bytes: &'a [u8], path: &Path) -> Result<&'a str, ToolError> {
    match std::str::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => Ok(text),
        _ => Err(ToolError::Refused(format!(
            "{} is a binary file, so it can't be shown as text.",
            path.display()
        ))),
    }
}

pub fn read(input: ReadInput, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
    let path = ctx.resolve_read(&input.file_path)?;
    // An open editor buffer, unsaved edits included, beats the file on disk (SPEC §11).
    let bytes = match ctx.editor_text(&path) {
        Some(text) => text.into_bytes(),
        None => read_bytes(&path)?,
    };
    let text = text_of(&bytes, &path)?;
    ctx.remember(&path, hash_of(&bytes));

    let total = text.lines().count();
    let first = input.offset.unwrap_or(1).max(1);
    let limit = input.limit.unwrap_or(DEFAULT_LINE_LIMIT);
    let mut content = String::new();
    for (index, line) in text.lines().enumerate().skip(first - 1).take(limit) {
        content.push_str(&format!("{:>6}\t{line}\n", index + 1));
    }
    let last = first.saturating_add(limit).saturating_sub(1).min(total);
    if first > 1 || last < total {
        content.push_str(&format!(
            "(lines {first}-{last} of {total}; use offset and limit for more)\n"
        ));
    }
    Ok(ToolOutput::ok(
        content,
        json!({ "filePath": path, "totalLines": total }),
    ))
}

/// Replaces `path` through a temp file in the same folder and a rename, so an interrupted
/// write leaves the original intact. The temp file gets the mode before any content, so a
/// private file is never briefly readable.
pub(crate) fn replace_with(
    path: &Path,
    fill: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let (temp, mut file) = loop {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp = dir.join(format!(".harness-tmp-{}-{n}", std::process::id()));
        // create_new never follows a symlink planted at the temp name.
        match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => break (temp, file),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    };
    let result = (|| {
        match fs::metadata(path) {
            Ok(meta) => file.set_permissions(meta.permissions())?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        fill(&mut file)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        // Best effort: the original error is the one worth reporting.
        let _ = fs::remove_file(&temp);
        return result;
    }
    // The rename has happened, so a failure here only risks durability after a power cut.
    File::open(dir)?.sync_all()
}

/// Refuses to overwrite `path` while the editor holds different text, since the write would
/// lose the user's unsaved edits. `buffer` is the editor's text, if any; `disk` is the file's
/// current content, read only by callers that have a buffer to compare.
// shortcut: refuse outright; buffer edits need fs/write_text_file, upgrade when that exists.
fn refuse_unsaved_buffer(path: &Path, buffer: Option<&str>, disk: &[u8]) -> Result<(), ToolError> {
    if buffer.is_some_and(|buffer| buffer.as_bytes() != disk) {
        return Err(ToolError::Refused(format!(
            "{} has unsaved changes in the editor; ask the user to save it.",
            path.display()
        )));
    }
    Ok(())
}

fn store(path: &Path, content: &str, ctx: &ToolContext) -> Result<(), ToolError> {
    ctx.checkpoint(path)?;
    replace_with(path, |file| file.write_all(content.as_bytes()))
        .map_err(ToolError::io("write", path))?;
    ctx.remember(path, hash_of(content.as_bytes()));
    Ok(())
}

pub fn write(input: WriteInput, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
    let path = resolve(&ctx.workspace, &input.file_path)?;
    check_writable(&ctx.workspace, &path)?;
    let existed = match fs::metadata(&path) {
        Ok(meta) if meta.is_file() => true,
        Ok(_) => {
            return Err(ToolError::Refused(format!(
                "{} is not a regular file.",
                path.display()
            )));
        }
        Err(_) => false,
    };
    // Without an editor buffer there is nothing to protect, so the old file is never read.
    if existed && let Some(buffer) = ctx.editor_text(&path) {
        let bytes = read_bytes(&path)?;
        refuse_unsaved_buffer(&path, Some(&buffer), &bytes)?;
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(ToolError::io("create the folder for", &path))?;
    }
    store(&path, &input.content, ctx)?;
    let lines = input.content.lines().count();
    Ok(ToolOutput::ok(
        format!("Wrote {} ({lines} lines).", path.display()),
        json!({
            "filePath": path,
            "type": if existed { "update" } else { "create" },
        }),
    ))
}

/// The first file line resembling `old`'s first line, ignoring surrounding whitespace.
fn near_match(text: &str, old: &str) -> Option<String> {
    let wanted = old.lines().map(str::trim).find(|l| !l.is_empty())?;
    let (index, line) = text
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains(wanted))?;
    Some(format!(
        " Line {} is close, check its whitespace: {line:?}",
        index + 1
    ))
}

pub fn edit(input: EditInput, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
    let path = resolve(&ctx.workspace, &input.file_path)?;
    check_writable(&ctx.workspace, &path)?;
    let shown = path.display();
    let refuse = |message: String| Err(ToolError::Refused(message));
    if input.old_string.is_empty() || input.old_string == input.new_string {
        return refuse(
            "old_string must not be empty and new_string must differ from it.".to_owned(),
        );
    }
    let Some(seen) = ctx.last_seen(&path) else {
        return refuse(format!("Read {shown} before editing it."));
    };
    let bytes = read_bytes(&path)?;
    refuse_unsaved_buffer(&path, ctx.editor_text(&path).as_deref(), &bytes)?;
    if hash_of(&bytes) != seen {
        return refuse(format!(
            "{shown} changed since you read it. Read it again before editing."
        ));
    }
    let text = text_of(&bytes, &path)?;

    let count = text.matches(&input.old_string).count();
    let replace_all = input.replace_all.unwrap_or(false);
    if count == 0 {
        let hint = near_match(text, &input.old_string).unwrap_or_default();
        return refuse(format!("old_string was not found in {shown}.{hint}"));
    }
    if count > 1 && !replace_all {
        return refuse(format!(
            "old_string is not unique in {shown}: it appears {count} times. \
             Add surrounding lines or set replace_all."
        ));
    }
    store(
        &path,
        &text.replace(&input.old_string, &input.new_string),
        ctx,
    )?;
    Ok(ToolOutput::ok(
        format!(
            "Edited {shown}: {count} replacement(s), {} line(s) became {}.",
            input.old_string.lines().count(),
            input.new_string.lines().count()
        ),
        json!({
            "filePath": path,
            "oldString": input.old_string,
            "newString": input.new_string,
            "replaceAll": replace_all,
        }),
    ))
}

/// What a `Write` or `Edit` would leave in a file, shown for review before the call runs.
#[derive(Debug, Clone, PartialEq)]
pub struct FileDiff {
    pub path: PathBuf,
    /// `None` for a file that doesn't exist yet.
    pub old: Option<String>,
    pub new: String,
}

/// The change `name` would make to its file, or `None` when the call wouldn't apply, so review
/// never shows a change that can't happen. Writes nothing; the call still checks everything.
pub fn proposed_diff(name: &str, input: &Value, workspace: &Path) -> Option<FileDiff> {
    match name {
        "Write" => {
            let input: WriteInput = serde_json::from_value(input.clone()).ok()?;
            let path = resolve(workspace, &input.file_path).ok()?;
            check_writable(workspace, &path).ok()?;
            let old = match fs::metadata(&path) {
                Ok(meta) if meta.is_file() => Some(current_text(&path)?),
                Ok(_) => return None,
                Err(_) => None,
            };
            Some(FileDiff {
                path,
                old,
                new: input.content,
            })
        }
        "Edit" => {
            let input: EditInput = serde_json::from_value(input.clone()).ok()?;
            let path = resolve(workspace, &input.file_path).ok()?;
            check_writable(workspace, &path).ok()?;
            if input.old_string.is_empty() || input.old_string == input.new_string {
                return None;
            }
            let old = current_text(&path)?;
            let count = old.matches(&input.old_string).count();
            if count == 0 || (count > 1 && !input.replace_all.unwrap_or(false)) {
                return None;
            }
            let new = old.replace(&input.old_string, &input.new_string);
            Some(FileDiff {
                path,
                old: Some(old),
                new,
            })
        }
        _ => None,
    }
}

fn current_text(path: &Path) -> Option<String> {
    let bytes = read_bytes(path).ok()?;
    text_of(&bytes, path).ok().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_write_leaves_the_original_intact_and_no_temp_file() {
        let dir = std::env::temp_dir().join(format!("harness-replace-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("file.txt");
        fs::write(&target, "original").unwrap();

        let result = replace_with(&target, |file| {
            file.write_all(b"parti")?;
            Err(io::Error::other("disk full"))
        });

        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "original");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A workspace with `name` holding `bytes`, and its context. The caller removes `dir`.
    fn workspace_with(name: &str, bytes: &[u8]) -> (PathBuf, PathBuf, ToolContext) {
        let dir = std::env::temp_dir().join(format!("harness-write-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join(name);
        fs::write(&target, bytes).unwrap();
        let ctx = ToolContext::new(&dir).unwrap();
        (dir, target, ctx)
    }

    #[cfg(unix)]
    #[test]
    fn write_without_an_editor_does_not_read_the_old_file() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, target, ctx) = workspace_with("out.txt", b"old");
        // Write-only: reading the old content would fail, but overwriting it must not read it.
        fs::set_permissions(&target, fs::Permissions::from_mode(0o200)).unwrap();

        let result = write(
            WriteInput {
                file_path: "out.txt".to_owned(),
                content: "new\n".to_owned(),
            },
            &ctx,
        );

        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            result.is_ok(),
            "write without an editor read the old file: {result:?}"
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "new\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_refuses_when_the_editor_holds_different_text() {
        let (dir, target, ctx) = workspace_with("dirty.txt", b"on disk\n");
        ctx.read_from_editor(super::super::EditorReader(Box::new(|_: &Path| {
            Some("unsaved edit\n".to_owned())
        })));

        let result = write(
            WriteInput {
                file_path: "dirty.txt".to_owned(),
                content: "agent\n".to_owned(),
            },
            &ctx,
        );

        assert!(
            matches!(result, Err(ToolError::Refused(ref m)) if m.contains("unsaved changes")),
            "expected a refusal, got {result:?}"
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "on disk\n");
        fs::remove_dir_all(&dir).unwrap();
    }
}
