//! Pure rendering for the panel, kept free of the DOM so it tests natively.

// One copy of the sanitizer for the panel and the plugin details tab (praxis-vscode).
#[path = "sanitize.rs"]
mod sanitize;
pub use sanitize::markdown;

/// Enter sends the prompt; Shift+Enter adds a newline and IME composition keeps its own Enter.
pub fn enter_sends(key: &str, shift: bool, composing: bool) -> bool {
    key == "Enter" && !shift && !composing
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

#[cfg(test)]
mod tests {
    use super::*;

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
