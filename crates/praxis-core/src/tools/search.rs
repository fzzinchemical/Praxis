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

/// Adds `line` unless it would take the lines past `room` characters. Returns false when full.
fn push_within(lines: &mut Vec<String>, used: &mut usize, line: String, room: usize) -> bool {
    let size = line.chars().count() + 1;
    if *used + size > room {
        return false;
    }
    *used += size;
    lines.push(line);
    true
}

fn finish(lines: Vec<String>, truncated: bool) -> ToolOutput {
    let count = lines.len();
    let mut content = lines.join("\n");
    if truncated {
        content.push_str(&format!(
            "\n(stopped at the result budget after {count} results; narrow the search)"
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
    let mut used = 0;
    let room = ctx.result_room();
    let mut full = false;
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
                    let text = format!("{shown}:{number}:{}", line.trim_end());
                    full |= !push_within(&mut lines, &mut used, text, room);
                    Ok(!full)
                }
                OutputMode::FilesWithMatches => Ok(false),
                OutputMode::Count => Ok(true),
            }
        });
        // Unreadable files are skipped, as above.
        if searcher.search_path(&matcher, path, sink).is_err() {
            continue;
        }
        // Checked before the match arms, which `continue` past it for Content mode.
        if full {
            break;
        }
        let text = match mode {
            OutputMode::FilesWithMatches if matches > 0 => shown.to_string(),
            OutputMode::Count if matches > 0 => format!("{shown}:{matches}"),
            _ => continue,
        };
        full |= !push_within(&mut lines, &mut used, text, room);
    }
    Ok(finish(lines, full))
}

/// Most paths a glob collects before it stops walking, so a huge tree cannot exhaust memory.
const GLOB_MAX_MATCHES: usize = 10_000;

pub fn glob(
    input: GlobInput,
    ctx: &ToolContext,
    cancel: &CancellationToken,
) -> Result<ToolOutput, ToolError> {
    glob_capped(input, ctx, cancel, GLOB_MAX_MATCHES)
}

fn glob_capped(
    input: GlobInput,
    ctx: &ToolContext,
    cancel: &CancellationToken,
    max_matches: usize,
) -> Result<ToolOutput, ToolError> {
    let root = search_root(input.path.as_deref(), ctx)?;
    let matcher = GlobBuilder::new(&input.pattern)
        .literal_separator(true)
        .build()
        .map_err(|e| ToolError::Refused(format!("Invalid pattern: {e}")))?
        .compile_matcher();
    let mut lines = Vec::new();
    // shortcut: past the cap the walk stops, so the sort orders only the first matches found, not all of them; upgrade to a bounded top-N if the order matters on huge trees.
    let mut walk_capped = false;
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
            if lines.len() >= max_matches {
                walk_capped = true;
                break;
            }
        }
    }
    lines.sort();
    let room = ctx.result_room();
    let mut kept = Vec::new();
    let mut used = 0;
    let mut full = walk_capped;
    for line in lines {
        if !push_within(&mut kept, &mut used, line, room) {
            full = true;
            break;
        }
    }
    Ok(finish(kept, full))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Many files with the same match, and a budget that fits only a few of them.
    fn crowded_workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("praxis-grep-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for n in 0..200 {
            fs::write(dir.join(format!("f{n:03}.txt")), "needle here\n").unwrap();
        }
        dir
    }

    #[test]
    fn glob_stops_at_the_match_cap_and_reports_truncation() {
        let dir = crowded_workspace("glob-cap");
        let ctx = ToolContext::new(&dir).unwrap();
        let input = GlobInput {
            pattern: "*.txt".to_owned(),
            path: None,
        };
        let out = glob_capped(input, &ctx, &CancellationToken::new(), 50).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(out.structured["truncated"], true);
        assert_eq!(out.structured["numResults"], 50);
    }

    #[test]
    fn content_grep_stops_at_the_budget_and_reports_truncation() {
        let dir = crowded_workspace("budget");
        let ctx = ToolContext::new(&dir).unwrap();
        ctx.set_result_chars(400);
        let input = GrepInput {
            pattern: "needle".to_owned(),
            path: None,
            glob: None,
            output_mode: None,
            case_insensitive: None,
            multiline: None,
        };
        let out = grep(input, &ctx, &CancellationToken::new()).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(out.structured["truncated"], true);
        let shown = out.structured["numResults"].as_u64().unwrap();
        assert!(shown > 0 && shown < 200, "shown {shown}");
        assert!(out.content.contains(&format!("after {shown} results")));
    }
}
