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
use std::ops::Range;
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
    // Stops before the budget, at a line boundary, so `fit_result` never cuts or spools a Read.
    let room = ctx.result_room();
    let mut content = String::new();
    let mut used = 0;
    let mut last = first - 1;
    for (index, line) in text.lines().enumerate().skip(first - 1).take(limit) {
        let numbered = format!("{:>6}\t{line}\n", index + 1);
        let size = numbered.chars().count();
        if used + size > room {
            // shortcut: a line longer than the budget is cut and its rest is unreachable; add byte offsets to Read if real files need it.
            if content.is_empty() {
                content = numbered
                    .chars()
                    .take(room.saturating_sub(1))
                    .collect::<String>()
                    + "\n";
                last = index + 1;
            }
            break;
        }
        content.push_str(&numbered);
        used += size;
        last = index + 1;
    }
    let last = last.min(total);
    if last < total {
        content.push_str(&format!(
            "(lines {first}-{last} of {total}; use offset {} for more)\n",
            last + 1
        ));
    } else if first > 1 {
        content.push_str(&format!("(lines {first}-{last} of {total})\n"));
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
        let temp = dir.join(format!(".praxis-tmp-{}-{n}", std::process::id()));
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
    let mut output = ToolOutput::ok(
        format!("Wrote {} ({lines} lines).", path.display()),
        json!({
            "filePath": path,
            "type": if existed { "update" } else { "create" },
        }),
    );
    ctx.add_problems(&mut output, &path);
    Ok(output)
}

/// Folds the typographic characters a model often gets wrong to their ASCII forms.
fn normalise(line: &str) -> String {
    line.chars()
        .map(|c| match c {
            '\u{2018}'..='\u{201B}' => '\'',
            '\u{201C}'..='\u{201F}' => '"',
            '\u{2010}'..='\u{2015}' => '-',
            '\u{00A0}' => ' ',
            c => c,
        })
        .collect()
}

/// Byte ranges where `old` occurs: exact matches first, then whole lines after `normalise`, then
/// the same with surrounding whitespace ignored. A looser tier is tried only when the stricter one
/// finds nothing, so a unique strict match is never lost to a loose one.
fn find_spans(text: &str, old: &str) -> Vec<Range<usize>> {
    let exact: Vec<Range<usize>> = text
        .match_indices(old)
        .map(|(start, found)| start..start + found.len())
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    for trim in [false, true] {
        let spans = line_spans(text, old, trim);
        if !spans.is_empty() {
            return spans;
        }
    }
    Vec::new()
}

/// Non-overlapping runs of whole lines in `text` whose normalised text equals `old`'s.
fn line_spans(text: &str, old: &str, trim: bool) -> Vec<Range<usize>> {
    let key = |line: &str| {
        let line = normalise(line);
        if trim { line.trim().to_owned() } else { line }
    };
    let lines = line_bounds(text);
    let keys: Vec<String> = lines
        .iter()
        .map(|l| key(&text[l.content.clone()]))
        .collect();
    let wanted: Vec<String> = old.lines().map(key).collect();
    let mut spans = Vec::new();
    let mut i = 0;
    while i + wanted.len() <= keys.len() {
        if keys[i..i + wanted.len()] == wanted[..] {
            let last = &lines[i + wanted.len() - 1];
            // A trailing newline in `old` also takes the matched line's terminator.
            let end = if old.ends_with('\n') {
                last.terminated.end
            } else {
                last.content.end
            };
            spans.push(lines[i].content.start..end);
            i += wanted.len();
        } else {
            i += 1;
        }
    }
    spans
}

/// Where a line's text and its terminator sit in the file.
struct LineBounds {
    content: Range<usize>,
    terminated: Range<usize>,
}

fn line_bounds(text: &str) -> Vec<LineBounds> {
    let mut start = 0;
    text.split_inclusive('\n')
        .map(|segment| {
            let content_end = start + segment.trim_end_matches(['\n', '\r']).len();
            let bounds = LineBounds {
                content: start..content_end,
                terminated: start..start + segment.len(),
            };
            start += segment.len();
            bounds
        })
        .collect()
}

/// The block of lines that best matches `old`, numbered, so the retry can copy it exactly.
fn near_match(text: &str, old: &str) -> String {
    let key = |line: &str| normalise(line).trim().to_owned();
    let wanted: Vec<String> = old.lines().map(key).collect();
    let lines: Vec<&str> = text.lines().collect();
    let keys: Vec<String> = lines.iter().map(|l| key(l)).collect();
    let best = (0..keys.len())
        .map(|start| {
            let score = wanted
                .iter()
                .enumerate()
                .filter(|(k, w)| keys.get(start + k) == Some(w))
                .count();
            (score, start)
        })
        .filter(|(score, _)| *score > 0)
        .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let Some((_, start)) = best else {
        return String::new();
    };
    let end = (start + wanted.len()).min(lines.len());
    let block: String = (start..end)
        .map(|i| format!("\n{:>6} | {}", i + 1, lines[i]))
        .collect();
    format!(
        " The closest block is at lines {}-{}:{block}",
        start + 1,
        end
    )
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

    let spans = find_spans(text, &input.old_string);
    let count = spans.len();
    let replace_all = input.replace_all.unwrap_or(false);
    if count == 0 {
        let hint = near_match(text, &input.old_string);
        return refuse(format!("old_string was not found in {shown}.{hint}"));
    }
    if count > 1 && !replace_all {
        return refuse(format!(
            "old_string is not unique in {shown}: it appears {count} times. \
             Add surrounding lines or set replace_all."
        ));
    }
    // A CRLF file gets CRLF in the new text, whatever line endings the model used.
    let crlf = spans.iter().any(|span| text[span.clone()].contains("\r\n"));
    let new = if crlf {
        input.new_string.replace("\r\n", "\n").replace('\n', "\r\n")
    } else {
        input.new_string.clone()
    };
    let mut updated = String::with_capacity(text.len());
    let mut last = 0;
    for span in &spans {
        updated.push_str(&text[last..span.start]);
        updated.push_str(&new);
        last = span.end;
    }
    updated.push_str(&text[last..]);
    store(&path, &updated, ctx)?;
    let mut output = ToolOutput::ok(
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
    );
    ctx.add_problems(&mut output, &path);
    Ok(output)
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
        let dir = std::env::temp_dir().join(format!("praxis-replace-{}", std::process::id()));
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
        let dir = std::env::temp_dir().join(format!("praxis-write-{name}-{}", std::process::id()));
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

    #[test]
    fn a_write_reports_the_editors_errors_and_caps_their_lines() {
        let (dir, _, ctx) = workspace_with("bad.rs", b"");
        let problems: Vec<String> = (1..=25).map(|n| format!("line {n}: bad")).collect();
        ctx.report_diagnostics(super::super::DiagnosticsReader(Box::new(
            move |_: &Path| Some(problems.clone()),
        )));

        let output = write(
            WriteInput {
                file_path: "bad.rs".to_owned(),
                content: "fn".to_owned(),
            },
            &ctx,
        )
        .unwrap();

        assert!(
            output.content.contains("\n- line 20: bad"),
            "{}",
            output.content
        );
        assert!(
            !output.content.contains("line 21: bad"),
            "{}",
            output.content
        );
        assert!(
            output.content.ends_with("... and 5 more"),
            "{}",
            output.content
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// Reads `name` as the model would, then runs `Edit` on it. The caller removes `dir`.
    fn edit_after_read(
        name: &str,
        bytes: &[u8],
        old: &str,
        new: &str,
    ) -> (PathBuf, PathBuf, Result<ToolOutput, ToolError>) {
        let (dir, target, ctx) = workspace_with(name, bytes);
        read(
            ReadInput {
                file_path: name.to_owned(),
                offset: None,
                limit: None,
            },
            &ctx,
        )
        .unwrap();
        let result = edit(
            EditInput {
                file_path: name.to_owned(),
                old_string: old.to_owned(),
                new_string: new.to_owned(),
                replace_all: None,
            },
            &ctx,
        );
        (dir, target, result)
    }

    #[test]
    fn edit_applies_a_block_that_differs_only_in_line_endings() {
        let (dir, target, result) = edit_after_read(
            "crlf.rs",
            b"fn a() {\r\n    x();\r\n}\r\n",
            "fn a() {\n    x();\n}\n",
            "fn a() {\n    y();\n}\n",
        );

        result.unwrap();
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "fn a() {\r\n    y();\r\n}\r\n"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn edit_applies_when_quotes_and_dashes_differ_from_the_file() {
        let (dir, target, result) = edit_after_read(
            "typography.txt",
            "let s = \u{201C}hi\u{201D} \u{2013} ok;\n".as_bytes(),
            "let s = \"hi\" - ok;",
            "let s = 1;",
        );

        result.unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "let s = 1;\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn edit_applies_when_only_surrounding_whitespace_differs() {
        let (dir, target, result) = edit_after_read(
            "indent.rs",
            b"fn a() {\n    b();\n}\n\nfn c() {\n    d();\n}\n",
            "  d();\n  }",
            "    e();\n}",
        );

        result.unwrap();
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "fn a() {\n    b();\n}\n\nfn c() {\n    e();\n}\n"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn edit_refuses_a_tolerant_match_that_is_not_unique() {
        let (dir, target, result) =
            edit_after_read("twice.txt", b"k\r\nv\r\nk\r\nv\r\n", "k\nv\n", "w\n");

        assert!(
            matches!(result, Err(ToolError::Refused(ref m)) if m.contains("not unique")),
            "expected a uniqueness refusal, got {result:?}"
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "k\r\nv\r\nk\r\nv\r\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_edit_shows_the_whole_closest_block_with_line_numbers() {
        let (dir, _, result) = edit_after_read(
            "miss.rs",
            b"fn a() {\n    b();\n}\n\nfn c() {\n    d();\n}\n",
            "fn c() {\n    e();\n}",
            "fn c() {}",
        );

        let Err(ToolError::Refused(message)) = result else {
            panic!("expected a refusal, got {result:?}");
        };
        assert!(message.contains("5 | fn c() {"), "{message}");
        assert!(message.contains("6 |     d();"), "{message}");
        assert!(message.contains("7 | }"), "{message}");
        fs::remove_dir_all(&dir).unwrap();
    }
}
