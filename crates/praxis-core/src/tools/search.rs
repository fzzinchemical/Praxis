//! `Grep` and `Glob`. Walks never follow symlinks, so they stay inside the search root.

use super::confine::resolve;
use super::{ToolContext, ToolError, ToolOutput};
use globset::GlobBuilder;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, SearcherBuilder, sinks::Lossy};
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

// shortcut: a flat cap with a note, so a huge search stops early instead of building a result only to spool it; page results if models need more.
const MAX_RESULTS: usize = 1000;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrepInput {
    pattern: String,
    /// File or folder to search. Defaults to the workspace.
    path: Option<String>,
    /// Only search files matching this glob, such as `*.rs`.
    glob: Option<String>,
    /// `content` (default) shows matching lines with line numbers, `files_with_matches` lists files, `count` counts matches per file.
    output_mode: Option<OutputMode>,
    /// Ignore case.
    #[serde(rename = "-i")]
    case_insensitive: Option<bool>,
    /// Let the pattern span lines, with `.` matching newlines.
    multiline: Option<bool>,
}

#[derive(Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum OutputMode {
    // Unlike Claude Code: small models rarely ask for lines, then grep again to find where to read.
    #[default]
    Content,
    FilesWithMatches,
    Count,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GlobInput {
    pattern: String,
    /// Folder to search. Defaults to the workspace.
    path: Option<String>,
}

fn walker(root: &Path) -> WalkBuilder {
    let mut walk = WalkBuilder::new(root);
    // Honour .gitignore in a workspace that isn't a repository yet.
    walk.require_git(false);
    walk
}

fn search_root(path: Option<&str>, ctx: &ToolContext) -> Result<PathBuf, ToolError> {
    let root = resolve(&ctx.workspace, path.unwrap_or("."))?;
    root.symlink_metadata()
        .map_err(ToolError::io("search", &root))?;
    Ok(root)
}

fn relative<'a>(path: &'a Path, ctx: &ToolContext) -> std::path::Display<'a> {
    path.strip_prefix(&ctx.workspace).unwrap_or(path).display()
}

fn finish(mut lines: Vec<String>) -> ToolOutput {
    let truncated = lines.len() >= MAX_RESULTS;
    lines.truncate(MAX_RESULTS);
    let count = lines.len();
    let mut content = lines.join("\n");
    if truncated {
        content.push_str(&format!(
            "\n(stopped at {MAX_RESULTS} results; narrow the search)"
        ));
    } else if count == 0 {
        content.push_str("No matches.");
    }
    ToolOutput::ok(
        content,
        json!({ "numResults": count, "truncated": truncated }),
    )
}

pub fn grep(
    input: GrepInput,
    ctx: &ToolContext,
    cancel: &CancellationToken,
) -> Result<ToolOutput, ToolError> {
    let bad = |e: &dyn std::fmt::Display| ToolError::Refused(format!("Invalid pattern: {e}"));
    let root = search_root(input.path.as_deref(), ctx)?;
    let multiline = input.multiline.unwrap_or(false);
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(input.case_insensitive.unwrap_or(false))
        .multi_line(multiline)
        .dot_matches_new_line(multiline)
        .build(&input.pattern)
        .map_err(|e| bad(&e))?;
    let mut searcher = SearcherBuilder::new()
        .multi_line(multiline)
        .binary_detection(BinaryDetection::quit(0))
        .build();

    let mut walk = walker(&root);
    if let Some(glob) = &input.glob {
        let mut overrides = OverrideBuilder::new(&root);
        overrides.add(glob).map_err(|e| bad(&e))?;
        walk.overrides(overrides.build().map_err(|e| bad(&e))?);
    }

    let mode = input.output_mode.unwrap_or_default();
    let mut lines = Vec::new();
    for entry in walk.build() {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        // An unreadable entry shouldn't hide matches elsewhere.
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let shown = relative(path, ctx);
        let mut matches = 0_usize;
        let sink = Lossy(|number, line: &str| {
            matches += 1;
            match mode {
                OutputMode::Content => {
                    lines.push(format!("{shown}:{number}:{}", line.trim_end()));
                    Ok(lines.len() < MAX_RESULTS)
                }
                OutputMode::FilesWithMatches => Ok(false),
                OutputMode::Count => Ok(true),
            }
        });
        // Unreadable files are skipped, as above.
        if searcher.search_path(&matcher, path, sink).is_err() {
            continue;
        }
        match mode {
            OutputMode::FilesWithMatches if matches > 0 => lines.push(shown.to_string()),
            OutputMode::Count if matches > 0 => lines.push(format!("{shown}:{matches}")),
            _ => {}
        }
        if lines.len() >= MAX_RESULTS {
            break;
        }
    }
    Ok(finish(lines))
}

pub fn glob(
    input: GlobInput,
    ctx: &ToolContext,
    cancel: &CancellationToken,
) -> Result<ToolOutput, ToolError> {
    let root = search_root(input.path.as_deref(), ctx)?;
    let matcher = GlobBuilder::new(&input.pattern)
        .literal_separator(true)
        .build()
        .map_err(|e| ToolError::Refused(format!("Invalid pattern: {e}")))?
        .compile_matcher();
    let mut lines = Vec::new();
    for entry in walker(&root).build() {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if entry.file_type().is_some_and(|t| t.is_file())
            && matcher.is_match(path.strip_prefix(&root).unwrap_or(path))
        {
            lines.push(relative(path, ctx).to_string());
        }
    }
    lines.sort();
    Ok(finish(lines))
}
