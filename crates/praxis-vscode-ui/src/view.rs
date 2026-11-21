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

/// The word being typed at the end of the prompt: the text after its last whitespace, empty when
/// the prompt ends in a space.
pub fn current_word(prompt: &str) -> &str {
    prompt
        .rsplit(char::is_whitespace)
        .next()
        .unwrap_or_default()
}

/// The prompt with its current word replaced by `/name `, so only that word changes.
pub fn with_command(prompt: &str, name: &str) -> String {
    let start = prompt.len() - current_word(prompt).len();
    format!("{}/{name} ", &prompt[..start])
}

/// Commands whose name contains what follows a `/` starting the word being typed, grouped by
/// source in first-seen order. Empty when that word does not start with `/`, so a slash in the
/// middle of a sentence still suggests, but a finished command's arguments do not.
// shortcut: substring match, not ranked; upgrade if long plugin lists make the best match hard to find.
pub fn suggestions<'a>(commands: &'a [Command], prompt: &str) -> Vec<(&'a str, Vec<&'a Command>)> {
    let Some(query) = current_word(prompt).strip_prefix('/') else {
        return Vec::new();
    };
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

/// Names for subagents, in the order a session starts them; the list wraps after the last one.
pub const SUBAGENT_NAMES: [&str; 10] = [
    "Blinky", "Pinky", "Inky", "Clyde", "Red", "Blue", "Green", "Yellow", "Purple", "Teal",
];

/// The name of the `index`th subagent a session started (0-based).
pub fn subagent_name(index: usize) -> &'static str {
    SUBAGENT_NAMES[index % SUBAGENT_NAMES.len()]
}

/// The one-line summary of a subagent's `Agent` call block. The caller sets it as text, never HTML,
/// since the type name comes from the model.
/// `usage` is the subagent's (prompt tokens, context length), once it has reported them.
pub fn subagent_summary(
    name: &str,
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
    let line = format!("{name} · {kind} · {calls} {noun} · {state}");
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

/// A tool call's status in the words its card shows.
pub fn tool_state(status: &str) -> &'static str {
    match status {
        "pending" | "in_progress" => "running",
        "completed" => "ok",
        "failed" => "error",
        _ => "unknown",
    }
}

/// How long a finished call took: milliseconds under a second, else seconds to one decimal.
pub fn duration_label(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", ms as f64 / 1000.0)
    }
}

/// A tool card's one-line summary: title, key argument, status, and duration once it has finished.
pub fn tool_line(title: &str, arg: Option<&str>, status: &str, duration_ms: Option<u64>) -> String {
    let mut parts = vec![title.to_owned()];
    parts.extend(arg.filter(|arg| !arg.is_empty()).map(str::to_owned));
    parts.push(tool_state(status).to_owned());
    parts.extend(duration_ms.map(duration_label));
    parts.join(" \u{b7} ")
}

/// The class of a diff line in a Write or Edit result: `add`, `del`, `hunk`, or empty for context.
pub fn diff_kind(line: &str) -> &'static str {
    if line.starts_with("@@") {
        "hunk"
    } else if line.starts_with('+') && !line.starts_with("+++") {
        "add"
    } else if line.starts_with('-') && !line.starts_with("---") {
        "del"
    } else {
        ""
    }
}

/// The resource line: host RAM and GPU VRAM as used / total GiB. A pool the host did not report
/// (a null in the snapshot) reads "n/a" rather than an error.
pub fn resources_line(ram: &serde_json::Value, vram: &serde_json::Value) -> String {
    format!("RAM {} \u{b7} VRAM {}", pool_label(ram), pool_label(vram))
}

fn pool_label(pool: &serde_json::Value) -> String {
    match (pool["used"].as_u64(), pool["total"].as_u64()) {
        (Some(used), Some(total)) => format!("{} / {} GiB", gib(used), gib(total)),
        _ => "n/a".to_owned(),
    }
}

fn gib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / 1_073_741_824.0)
}

/// Token counts in thousands, rounded to the nearest; counts under 1000 stay exact.
fn tokens_k(count: u32) -> String {
    if count < 1000 {
        count.to_string()
    } else {
        format!("{}k", (count + 500) / 1000)
    }
}

/// What the agent is doing. Only `Idle` matters to the UI: it gates sending and the pickers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Phase {
    Idle,
    ProcessingPrompt,
    Generating,
    RunningTool,
    /// A permission prompt is waiting for the user's answer.
    Waiting,
}

/// What the status bar says the agent is doing; `running` names the tool in flight, if any.
pub fn state_label(phase: &Phase, running: Option<&str>) -> String {
    match phase {
        Phase::Idle => "Idle".to_owned(),
        Phase::ProcessingPrompt => "Thinking".to_owned(),
        Phase::Generating => "Writing".to_owned(),
        Phase::RunningTool => {
            running.map_or_else(|| "Running a tool".to_owned(), |r| format!("Running {r}"))
        }
        Phase::Waiting => "Waiting for your answer".to_owned(),
    }
}

/// The status bar's line: model, state, and the context in use once the agent has reported it.
/// The caller sets it as text, since the model name and tool names come from the agent.
pub fn status_line(model: &str, state: &str, usage: Option<(u32, u32)>) -> String {
    let model = if model.is_empty() { "no model" } else { model };
    match usage {
        Some((used, size)) if size > 0 => {
            format!(
                "{model} \u{b7} {state} \u{b7} {} / {} tokens",
                tokens_k(used),
                tokens_k(size)
            )
        }
        _ => format!("{model} \u{b7} {state}"),
    }
}

/// The playful words the busy indicator cycles through, one every two seconds: Pac-Man words mixed
/// with thinking words.
pub const WORKING_WORDS: [&str; 30] = [
    "Chomping",
    "Munching",
    "Nibbling",
    "Gobbling",
    "Crunching",
    "Snacking",
    "Devouring",
    "Nomming",
    "Gulping",
    "Dot-hunting",
    "Pellet-popping",
    "Maze-running",
    "Ghost-dodging",
    "Power-pelleting",
    "Fruit-hunting",
    "Pondering",
    "Mulling",
    "Ruminating",
    "Brewing",
    "Scheming",
    "Tinkering",
    "Noodling",
    "Percolating",
    "Puzzling",
    "Hatching",
    "Weighing",
    "Sifting",
    "Contemplating",
    "Reasoning",
    "Musing",
];

/// The indicator's word `elapsed_secs` into a turn. `offset` (the turn number, from the caller)
/// shifts the start so consecutive turns don't open on the same word. Reduced motion keeps one word.
pub fn working_word(elapsed_secs: u64, offset: usize, reduced_motion: bool) -> &'static str {
    if reduced_motion {
        return "Working";
    }
    WORKING_WORDS[((elapsed_secs / 2) as usize + offset) % WORKING_WORDS.len()]
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

/// Whether the transcript was close enough to its bottom that new output should keep it there.
/// shortcut: fixed 40px slack for rounding, so a reader within a line of the bottom still follows.
pub fn at_bottom(scroll_top: f64, client_height: f64, scroll_height: f64) -> bool {
    scroll_height - scroll_top - client_height <= 40.0
}

/// Whether the transcript keeps following the bottom after a scroll event. Near the bottom it always
/// follows; otherwise only an upward move (the reader scrolling away) releases it. Growth never moves
/// the top, and our own scrolls go down, so neither can clear the flag.
pub fn next_follow(
    was_following: bool,
    previous_top: f64,
    scroll_top: f64,
    client_height: f64,
    scroll_height: f64,
) -> bool {
    if at_bottom(scroll_top, client_height, scroll_height) {
        true
    } else if scroll_top < previous_top {
        false
    } else {
        was_following
    }
}

/// A profile value as the popover's input shows it: strings as they are, lists and numbers as TOML
/// (what `profile set` reads back), and nothing for an unset value.
pub fn profile_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_transcript_follows_only_when_at_or_near_the_bottom() {
        // 1000px of content in a 400px view: at the bottom, 40px up, and 41px up.
        assert!(at_bottom(600.0, 400.0, 1000.0));
        assert!(at_bottom(560.0, 400.0, 1000.0));
        assert!(!at_bottom(559.0, 400.0, 1000.0));
        assert!(!at_bottom(0.0, 400.0, 1000.0));
    }

    #[test]
    fn a_transcript_shorter_than_its_view_always_follows() {
        assert!(at_bottom(0.0, 400.0, 120.0));
    }

    #[test]
    fn only_scrolling_up_releases_the_follow_flag() {
        // Content grew below the reader (scroll top unchanged, now 41px short of the bottom): keep following.
        assert!(next_follow(true, 600.0, 600.0, 400.0, 1100.0));
        // Our own scroll-down, still short of the bottom: keep following.
        assert!(next_follow(true, 100.0, 300.0, 400.0, 1500.0));
        // The reader scrolls up away from the bottom: stop following.
        assert!(!next_follow(true, 600.0, 300.0, 400.0, 1000.0));
        // Back at the bottom re-arms it, and a scroll-down while released stays released.
        assert!(next_follow(false, 300.0, 600.0, 400.0, 1000.0));
        assert!(!next_follow(false, 100.0, 200.0, 400.0, 1000.0));
    }

    #[test]
    fn profile_values_show_as_text_and_unset_as_empty() {
        assert_eq!(profile_text(&serde_json::Value::Null), "");
        assert_eq!(profile_text(&serde_json::json!("chatml")), "chatml");
        assert_eq!(profile_text(&serde_json::json!(4096)), "4096");
        assert_eq!(profile_text(&serde_json::json!(["a", "b"])), r#"["a","b"]"#);
    }

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
    fn gfm_tables_render_as_html_tables() {
        let out = markdown("| a | b |\n|---|:-:|\n| 1 | 2 |");
        assert!(out.contains("<table>"), "{out}");
        assert!(out.contains("<th>a</th>"), "{out}");
        assert!(out.contains("<td>1</td>"), "{out}");
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

    fn names(commands: &[Command], prompt: &str) -> Vec<String> {
        suggestions(commands, prompt)
            .iter()
            .flat_map(|(_, items)| items.iter().map(|c| c.name.clone()))
            .collect()
    }

    #[test]
    fn a_slash_starting_the_word_being_typed_suggests_even_mid_prompt() {
        let commands = [command("ponytail:lite", "ponytail"), command("deploy", "d")];
        assert_eq!(names(&commands, "please run /pon"), ["ponytail:lite"]);
        assert_eq!(names(&commands, "/dep and then /pony"), ["ponytail:lite"]);
        assert_eq!(names(&commands, "run /"), ["ponytail:lite", "deploy"]);
    }

    #[test]
    fn no_suggestions_once_the_command_word_is_finished_or_has_no_slash() {
        let commands = [command("deploy", "d")];
        assert!(suggestions(&commands, "deploy").is_empty());
        assert!(suggestions(&commands, "please deploy").is_empty());
        assert_eq!(names(&commands, "please /deploy"), ["deploy"]);
        assert!(suggestions(&commands, "/deploy ").is_empty());
        assert!(suggestions(&commands, "/deploy now").is_empty());
    }

    #[test]
    fn accepting_a_suggestion_replaces_only_the_word_being_typed() {
        assert_eq!(with_command("/pon", "ponytail:lite"), "/ponytail:lite ");
        assert_eq!(
            with_command("please run /pon", "ponytail:lite"),
            "please run /ponytail:lite "
        );
        assert_eq!(
            with_command("keep this, /dep", "deploy"),
            "keep this, /deploy "
        );
    }

    #[test]
    fn a_subagent_summary_names_its_name_type_calls_and_status() {
        assert_eq!(
            subagent_summary("Blinky", "Explore", 6, "in_progress", None),
            "Blinky · Explore · 6 tool calls · running"
        );
        assert_eq!(
            subagent_summary("Pinky", "general", 1, "completed", None),
            "Pinky · general · 1 tool call · completed"
        );
        assert_eq!(
            subagent_summary("Inky", "general", 0, "failed", None),
            "Inky · general · 0 tool calls · failed"
        );
    }

    #[test]
    fn subagents_take_the_names_in_order_and_wrap_after_the_list() {
        let names: Vec<&str> = (0..12).map(subagent_name).collect();
        assert_eq!(
            names,
            [
                "Blinky", "Pinky", "Inky", "Clyde", "Red", "Blue", "Green", "Yellow", "Purple",
                "Teal", "Blinky", "Pinky"
            ]
        );
    }

    #[test]
    fn a_subagent_summary_shows_its_prompt_size_against_the_context() {
        assert_eq!(
            subagent_summary("Clyde", "general", 2, "in_progress", Some((12_000, 32_000))),
            "Clyde · general · 2 tool calls · running · 12k / 32k tokens"
        );
    }

    #[test]
    fn working_word_cycles_every_two_seconds_and_wraps() {
        assert_eq!(working_word(0, 0, false), "Chomping");
        assert_eq!(working_word(1, 0, false), "Chomping");
        assert_eq!(working_word(2, 0, false), "Munching");
        assert_eq!(working_word(60, 0, false), "Chomping");
    }

    #[test]
    fn a_turn_offset_starts_the_cycle_on_a_different_word() {
        assert_eq!(working_word(0, 1, false), "Munching");
        assert_eq!(working_word(0, 2, false), "Nibbling");
        assert_eq!(working_word(0, 30, false), "Chomping");
    }

    #[test]
    fn reduced_motion_keeps_one_static_word() {
        assert_eq!(working_word(0, 0, true), "Working");
        assert_eq!(working_word(7, 3, true), "Working");
    }

    #[test]
    fn a_tool_card_names_its_argument_status_and_duration() {
        assert_eq!(
            tool_line("Edit", Some("/w/a.rs"), "in_progress", None),
            "Edit \u{b7} /w/a.rs \u{b7} running"
        );
        assert_eq!(
            tool_line("Bash", Some("cargo test"), "completed", Some(1_250)),
            "Bash \u{b7} cargo test \u{b7} ok \u{b7} 1.2 s"
        );
        assert_eq!(
            tool_line("Glob", None, "failed", Some(40)),
            "Glob \u{b7} error \u{b7} 40 ms"
        );
        assert_eq!(
            tool_line("Read", Some(""), "weird", None),
            "Read \u{b7} unknown"
        );
    }

    #[test]
    fn the_status_line_shows_model_state_and_context_when_known() {
        assert_eq!(
            status_line("qwen3.5-4b", &state_label(&Phase::Idle, None), None),
            "qwen3.5-4b \u{b7} Idle"
        );
        assert_eq!(
            status_line(
                "qwen3.5-4b",
                &state_label(&Phase::RunningTool, Some("Edit /w/a.rs")),
                Some((12_400, 32_000))
            ),
            "qwen3.5-4b \u{b7} Running Edit /w/a.rs \u{b7} 12k / 32k tokens"
        );
        assert_eq!(
            status_line("", "Waiting for your answer", Some((5, 0))),
            "no model \u{b7} Waiting for your answer"
        );
        assert_eq!(
            state_label(&Phase::Waiting, None),
            "Waiting for your answer"
        );
        assert_eq!(state_label(&Phase::RunningTool, None), "Running a tool");
    }

    #[test]
    fn the_resource_line_shows_used_and_total_gib_and_n_a_when_unknown() {
        let gib = 1_073_741_824_u64;
        let ram = serde_json::json!({"used": 12 * gib + gib / 2, "total": 62 * gib});
        let vram = serde_json::json!({"used": 6 * gib, "total": 16 * gib});
        assert_eq!(
            resources_line(&ram, &vram),
            "RAM 12.5 / 62.0 GiB \u{b7} VRAM 6.0 / 16.0 GiB"
        );
        assert_eq!(
            resources_line(&ram, &serde_json::Value::Null),
            "RAM 12.5 / 62.0 GiB \u{b7} VRAM n/a"
        );
    }

    #[test]
    fn diff_lines_are_added_removed_or_hunk_headers_but_not_file_headers() {
        assert_eq!(diff_kind("@@ -1,2 +1,2 @@"), "hunk");
        assert_eq!(diff_kind("+let x = 1;"), "add");
        assert_eq!(diff_kind("-let x = 0;"), "del");
        assert_eq!(diff_kind("+++ b/a.rs"), "");
        assert_eq!(diff_kind("--- a/a.rs"), "");
        assert_eq!(diff_kind(" unchanged"), "");
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
