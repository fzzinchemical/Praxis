//! Pure rendering for the panel, kept free of the DOM so it tests natively.

// One copy of the sanitizer for the panel and the plugin details tab (praxis-vscode).
#[path = "sanitize.rs"]
mod sanitize;
pub use sanitize::markdown;

/// Enter sends the prompt; Shift+Enter adds a newline and IME composition keeps its own Enter.
pub fn enter_sends(key: &str, shift: bool, composing: bool) -> bool {
    key == "Enter" && !shift && !composing
}

/// A prompt is accepted only between turns, so the panel's turn numbers match the agent's (rewind).
pub fn can_send(ready: bool, phase: &Phase) -> bool {
    ready && *phase == Phase::Idle
}

/// A command or skill the agent announced (`available_commands_update`).
#[derive(Clone, Debug, PartialEq)]
pub struct Command {
    /// `<plugin>:<command>`, or a standalone skill's name.
    pub name: String,
    pub description: String,
    /// The plugin name, or the skill's directory for a standalone skill.
    pub source: String,
}

/// Commands whose name contains what follows a leading `/`, grouped by source in first-seen
/// order. Empty once the prompt has a space: the command is typed and the rest is its argument.
// shortcut: substring match, not ranked; upgrade if long plugin lists make the best match hard to find.
pub fn suggestions<'a>(commands: &'a [Command], prompt: &str) -> Vec<(&'a str, Vec<&'a Command>)> {
    let Some(query) = prompt.strip_prefix('/') else {
        return Vec::new();
    };
    if query.contains(char::is_whitespace) {
        return Vec::new();
    }
    let query = query.to_lowercase();
    let mut groups: Vec<(&'a str, Vec<&'a Command>)> = Vec::new();
    for command in commands
        .iter()
        .filter(|c| c.name.to_lowercase().contains(&query))
    {
        match groups
            .iter_mut()
            .find(|(source, _)| *source == command.source)
        {
            Some((_, items)) => items.push(command),
            None => groups.push((command.source.as_str(), vec![command])),
        }
    }
    groups
}

/// The one-line summary of a subagent's `Agent` call block. The caller sets it as text, never HTML,
/// since the type name comes from the model.
/// `usage` is the subagent's (prompt tokens, context length), once it has reported them.
pub fn subagent_summary(
    kind: &str,
    calls: usize,
    status: &str,
    usage: Option<(u32, u32)>,
) -> String {
    let state = match status {
        "pending" | "in_progress" => "running",
        "failed" => "failed",
        _ => "completed",
    };
    let noun = if calls == 1 {
        "tool call"
    } else {
        "tool calls"
    };
    let line = format!("{kind} · {calls} {noun} · {state}");
    match usage {
        Some((prompt, context)) => {
            format!(
                "{line} · {} / {} tokens",
                tokens_k(prompt),
                tokens_k(context)
            )
        }
        None => line,
    }
}

/// The files a subagent changed: the paths of its completed calls that have one (`Write` and
/// `Edit`), deduplicated in first-seen order. Each item is a call's path and status.
pub fn changed_files<'a>(
    calls: impl IntoIterator<Item = (Option<&'a str>, &'a str)>,
) -> Vec<&'a str> {
    let mut files: Vec<&'a str> = Vec::new();
    for (path, status) in calls {
        if let Some(path) = path.filter(|_| status == "completed")
            && !files.contains(&path)
        {
            files.push(path);
        }
    }
    files
}

/// One changed file in the change-set review, with the indices of its chunks (`Write` and `Edit`
/// calls, in arrival order) in the change list.
#[derive(Debug, PartialEq)]
pub struct FileGroup {
    pub file: String,
    pub chunks: Vec<usize>,
}

/// Groups `(file, title)` changes by file, files in first-seen order.
pub fn group_changes(changes: &[(&str, &str)]) -> Vec<FileGroup> {
    let mut groups: Vec<FileGroup> = Vec::new();
    for (index, (file, _)) in changes.iter().enumerate() {
        match groups.iter_mut().find(|group| group.file == *file) {
            Some(group) => group.chunks.push(index),
            None => groups.push(FileGroup {
                file: (*file).to_owned(),
                chunks: vec![index],
            }),
        }
    }
    groups
}

/// Descriptive labels for what one change touches. They point at places to look and never judge
/// the change, so no label may read as a verdict (see the test that checks the wording).
pub fn risk_labels(file: &str, title: &str) -> Vec<&'static str> {
    let parts: Vec<&str> = file.split('/').collect();
    let name = parts.last().copied().unwrap_or_default();
    let lower = file.to_ascii_lowercase();
    let mut labels = Vec::new();
    if title.starts_with("Write") {
        labels.push("rewrites the whole file");
    }
    if matches!(
        name,
        "Cargo.toml" | "build.rs" | "package.json" | "Makefile" | "justfile"
    ) || parts.contains(&".cargo")
    {
        labels.push("touches build config");
    }
    if matches!(
        name,
        "Cargo.lock" | "package-lock.json" | "yarn.lock" | "pnpm-lock.yaml"
    ) {
        labels.push("touches a lockfile");
    }
    if parts
        .iter()
        .any(|part| matches!(*part, ".github" | ".githooks" | "scripts"))
    {
        labels.push("touches CI or scripts");
    }
    if ["auth", "secret", "token", "credential", ".env"]
        .iter()
        .any(|word| lower.contains(word))
    {
        labels.push("touches auth or secrets");
    }
    labels
}

/// Token counts in thousands, rounded to the nearest; counts under 1000 stay exact.
fn tokens_k(count: u32) -> String {
    if count < 1000 {
        count.to_string()
    } else {
        format!("{}k", (count + 500) / 1000)
    }
}

/// What the agent is doing, for the status line.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Phase {
    Idle,
    ProcessingPrompt,
    Generating,
    RunningTool,
}

#[derive(Debug, PartialEq)]
pub struct Status {
    pub phase: Phase,
    /// Context tokens used and the window size, from the last `usage` message.
    pub context: Option<(u64, u64)>,
}

impl Status {
    pub fn line(&self, elapsed_secs: u64) -> String {
        let phase = match self.phase {
            Phase::Idle => "Idle",
            Phase::ProcessingPrompt => "Processing prompt",
            Phase::Generating => "Generating",
            Phase::RunningTool => "Running tool",
        };
        let time = format!("{}:{:02}", elapsed_secs / 60, elapsed_secs % 60);
        let context = match self.context {
            Some((used, size)) if size > 0 => {
                format!("context {used} / {size} ({}%)", used * 100 / size)
            }
            Some((used, size)) => format!("context {used} / {size}"),
            None => "context unknown".to_owned(),
        };
        format!("{phase} · {time} · {context}")
    }
}

/// The child index that a rewind to user turn `turn` (1-based) cuts the chat at: the `user`
/// bubble that starts it. None when the chat has no such turn.
pub fn cut_at_turn(classes: &[String], turn: usize) -> Option<usize> {
    classes
        .iter()
        .enumerate()
        .filter(|(_, class)| class.as_str() == "user")
        .map(|(index, _)| index)
        .nth(turn.checked_sub(1)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rewind_cuts_the_chat_at_the_user_bubble_of_that_turn() {
        let chat: Vec<String> = ["user", "message", "rewind", "user", "rewind", "notice"]
            .map(str::to_owned)
            .into();
        assert_eq!(cut_at_turn(&chat, 1), Some(0));
        assert_eq!(cut_at_turn(&chat, 2), Some(3));
        assert_eq!(cut_at_turn(&chat, 3), None, "no third turn");
        assert_eq!(cut_at_turn(&chat, 0), None, "turns start at 1");
    }

    #[test]
    fn script_tags_in_model_text_render_inert() {
        for text in [
            "<script>alert(1)</script>",
            "hi <script>alert(1)</script>",
            "<img src=x onerror=alert(1)>",
        ] {
            let out = markdown(text);
            assert!(!out.contains("<script"), "{out}");
            assert!(!out.contains("<img"), "{out}");
        }
    }

    #[test]
    fn javascript_links_render_inert() {
        for text in [
            "[x](javascript:alert(1))",
            "[x](JavaScript:alert(1))",
            "[x]: javascript:alert(1)\n\n[x]",
            "<javascript:alert(1)>",
            "![x](javascript:alert(1))",
            "[x](data:text/html,hi)",
        ] {
            let out = markdown(text);
            assert!(!out.contains("href"), "{text} -> {out}");
            assert!(!out.contains("src"), "{text} -> {out}");
            assert!(out.contains('x') || out.contains("javascript"), "{out}");
        }
    }

    #[test]
    fn safe_links_and_formatting_survive() {
        let out = markdown("**b** [docs](https://example.com) [v](vscode:extension/x) `c`");
        assert!(out.contains("<strong>b</strong>"), "{out}");
        assert!(out.contains(r#"href="https://example.com""#), "{out}");
        assert!(out.contains(r#"href="vscode:extension/x""#), "{out}");
        assert!(out.contains("<code>c</code>"), "{out}");
    }

    #[test]
    fn enter_sends_but_shift_enter_and_composition_do_not() {
        assert!(enter_sends("Enter", false, false));
        assert!(!enter_sends("Enter", true, false));
        assert!(!enter_sends("Enter", false, true));
        assert!(!enter_sends("a", false, false));
    }

    #[test]
    fn a_prompt_is_accepted_only_when_ready_and_idle() {
        assert!(can_send(true, &Phase::Idle));
        assert!(!can_send(false, &Phase::Idle));
        assert!(!can_send(true, &Phase::ProcessingPrompt));
        assert!(!can_send(true, &Phase::Generating));
        assert!(!can_send(true, &Phase::RunningTool));
    }

    fn command(name: &str, source: &str) -> Command {
        Command {
            name: name.into(),
            description: format!("{name} does things"),
            source: source.into(),
        }
    }

    #[test]
    fn a_slash_lists_every_command_grouped_by_source_in_first_seen_order() {
        let commands = [
            command("ponytail:lite", "ponytail"),
            command("deploy", "/home/u/.claude/skills"),
            command("ponytail:full", "ponytail"),
        ];
        let groups = suggestions(&commands, "/");
        let names: Vec<(&str, Vec<&str>)> = groups
            .iter()
            .map(|(source, items)| (*source, items.iter().map(|c| c.name.as_str()).collect()))
            .collect();
        assert_eq!(
            names,
            [
                ("ponytail", vec!["ponytail:lite", "ponytail:full"]),
                ("/home/u/.claude/skills", vec!["deploy"]),
            ]
        );
    }

    #[test]
    fn what_follows_the_slash_filters_by_substring_ignoring_case() {
        let commands = [command("ponytail:lite", "ponytail"), command("deploy", "d")];
        let names = |prompt| -> Vec<String> {
            suggestions(&commands, prompt)
                .iter()
                .flat_map(|(_, items)| items.iter().map(|c| c.name.clone()))
                .collect()
        };
        assert_eq!(names("/lit"), ["ponytail:lite"]);
        assert_eq!(names("/PONY"), ["ponytail:lite"]);
        assert_eq!(names("/ploy"), ["deploy"]);
        assert!(names("/zzz").is_empty());
    }

    #[test]
    fn no_suggestions_unless_the_prompt_starts_with_a_slash_and_the_name_is_still_being_typed() {
        let commands = [command("deploy", "d")];
        assert!(suggestions(&commands, "deploy").is_empty());
        assert!(suggestions(&commands, "please /deploy").is_empty());
        assert!(suggestions(&commands, "/deploy ").is_empty());
        assert!(suggestions(&commands, "/deploy now").is_empty());
    }

    #[test]
    fn a_subagent_summary_names_its_type_calls_and_status() {
        assert_eq!(
            subagent_summary("Explore", 6, "in_progress", None),
            "Explore · 6 tool calls · running"
        );
        assert_eq!(
            subagent_summary("general", 1, "completed", None),
            "general · 1 tool call · completed"
        );
        assert_eq!(
            subagent_summary("general", 0, "failed", None),
            "general · 0 tool calls · failed"
        );
    }

    #[test]
    fn a_subagent_summary_shows_its_prompt_size_against_the_context() {
        assert_eq!(
            subagent_summary("general", 2, "in_progress", Some((12_000, 32_000))),
            "general · 2 tool calls · running · 12k / 32k tokens"
        );
    }

    #[test]
    fn status_line_shows_phase_elapsed_time_and_context_used() {
        let status = Status {
            phase: Phase::Generating,
            context: Some((5_000, 32_768)),
        };
        assert_eq!(
            status.line(75),
            "Generating · 1:15 · context 5000 / 32768 (15%)"
        );
        let status = Status {
            phase: Phase::ProcessingPrompt,
            context: None,
        };
        assert_eq!(status.line(3), "Processing prompt · 0:03 · context unknown");
        let status = Status {
            phase: Phase::RunningTool,
            context: Some((0, 0)),
        };
        assert_eq!(status.line(0), "Running tool · 0:00 · context 0 / 0");
    }

    #[test]
    fn changes_group_under_their_file_in_first_seen_order() {
        let changes = [
            ("/w/a.rs", "Edit a.rs"),
            ("/w/b.rs", "Write b.rs"),
            ("/w/a.rs", "Edit a.rs"),
        ];
        let groups = group_changes(&changes);
        assert_eq!(
            groups,
            [
                FileGroup {
                    file: "/w/a.rs".to_owned(),
                    chunks: vec![0, 2]
                },
                FileGroup {
                    file: "/w/b.rs".to_owned(),
                    chunks: vec![1]
                },
            ]
        );
    }

    #[test]
    fn risk_labels_describe_what_a_change_touches() {
        assert_eq!(
            risk_labels("/w/Cargo.toml", "Edit Cargo.toml"),
            ["touches build config"]
        );
        assert_eq!(
            risk_labels("/w/.github/workflows/ci.yml", "Write ci.yml"),
            ["rewrites the whole file", "touches CI or scripts"]
        );
        assert_eq!(
            risk_labels("/w/src/auth/token.rs", "Edit token.rs"),
            ["touches auth or secrets"]
        );
        assert!(risk_labels("/w/src/lib.rs", "Edit lib.rs").is_empty());
    }

    #[test]
    fn risk_labels_never_read_as_a_safety_verdict() {
        // Words that would call a change checked, correct or harmless. Matched as whole words.
        const VERDICTS: [&str; 12] = [
            "safe", "safely", "ok", "okay", "low", "risk", "verified", "checked", "approved",
            "secure", "correct", "passed",
        ];
        let samples = [
            ("/w/Cargo.toml", "Write Cargo.toml"),
            ("/w/.cargo/config.toml", "Edit config.toml"),
            ("/w/Cargo.lock", "Edit Cargo.lock"),
            ("/w/.githooks/pre-commit", "Write pre-commit"),
            ("/w/scripts/verify", "Edit verify"),
            ("/w/.env", "Write .env"),
            ("/w/src/secret.rs", "Edit secret.rs"),
            ("/w/src/lib.rs", "Edit lib.rs"),
        ];
        let mut seen = 0;
        for (file, title) in samples {
            for label in risk_labels(file, title) {
                seen += 1;
                for word in label.split(|c: char| !c.is_ascii_alphabetic()) {
                    let word = word.to_ascii_lowercase();
                    assert!(
                        !VERDICTS.contains(&word.as_str()),
                        "verdict word in {label:?}"
                    );
                }
            }
        }
        assert!(seen > 0, "the samples must produce labels to check");
    }

    #[test]
    fn a_subagents_changed_files_are_its_completed_writes_in_first_seen_order() {
        let calls = [
            (Some("/w/a.rs"), "completed"),
            (Some("/w/b.rs"), "failed"),
            (None, "completed"),
            (Some("/w/c.rs"), "pending"),
            (Some("/w/a.rs"), "completed"),
            (Some("/w/d.rs"), "completed"),
        ];
        assert_eq!(changed_files(calls), ["/w/a.rs", "/w/d.rs"]);
    }
}
