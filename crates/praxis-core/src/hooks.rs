//! Plugin hooks (SPEC §12): `type: "command"` only, run with a scrubbed environment and a
//! timeout. Their output is untrusted; the permission check decides what it can change.

use crate::tools::bash::child_env;
use regex::Regex;
use serde_json::{Map, Value};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

const DEFAULT_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone)]
pub enum Matcher {
    All,
    Names(Vec<String>),
    Pattern(Regex),
}

impl Matcher {
    /// `None` when the pattern isn't valid Rust regex (Claude Code uses JS regex).
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() || text == "*" {
            return Some(Self::All);
        }
        let plain = |c: char| c.is_ascii_alphanumeric() || " _-,|".contains(c);
        if text.chars().all(plain) {
            let names = text.split([',', '|']).map(str::trim);
            return Some(Self::Names(
                names.filter(|n| !n.is_empty()).map(str::to_owned).collect(),
            ));
        }
        Regex::new(text).ok().map(Self::Pattern)
    }

    pub fn matches(&self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Names(names) => names.iter().any(|n| n == name),
            Self::Pattern(re) => re.is_match(name),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Hook {
    pub event: String,
    pub matcher: Matcher,
    pub command: String,
    /// `Some` runs `command` directly with these arguments; `None` runs it through the shell.
    pub args: Option<Vec<String>>,
    pub timeout: Duration,
}

/// Parses an event map (a hooks file's `"hooks"` value, or an inline manifest object).
/// Skipped events, types and matchers come back as warnings.
pub fn parse(events: &Value) -> (Vec<Hook>, Vec<String>) {
    let (mut hooks, mut warnings) = (Vec::new(), Vec::new());
    let Some(events) = events.as_object() else {
        return (
            hooks,
            vec!["hooks: expected an object of events".to_owned()],
        );
    };
    for (event, groups) in events {
        // shortcut: only PreToolUse runs; add each event here when Agent fires it (SPEC §12).
        if event != "PreToolUse" {
            warnings.push(format!("hooks: `{event}` doesn't run yet, skipped"));
            continue;
        }
        for group in groups.as_array().into_iter().flatten() {
            let text = group.get("matcher").and_then(Value::as_str).unwrap_or("");
            let Some(matcher) = Matcher::parse(text) else {
                warnings.push(format!(
                    "hooks: matcher `{text}` isn't a valid regex, skipped"
                ));
                continue;
            };
            for hook in group
                .get("hooks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let kind = hook.get("type").and_then(Value::as_str).unwrap_or("");
                let Some(command) = hook.get("command").and_then(Value::as_str) else {
                    warnings.push(format!("hooks: `{event}` hook of type `{kind}` skipped"));
                    continue;
                };
                if kind != "command" {
                    warnings.push(format!("hooks: `{event}` hook of type `{kind}` skipped"));
                    continue;
                }
                let args = hook.get("args").and_then(Value::as_array).map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                });
                let secs = hook.get("timeout").and_then(Value::as_u64);
                hooks.push(Hook {
                    event: event.clone(),
                    matcher: matcher.clone(),
                    command: command.to_owned(),
                    args,
                    timeout: Duration::from_secs(secs.unwrap_or(DEFAULT_TIMEOUT_SECS)),
                });
            }
        }
    }
    (hooks, warnings)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Deny,
    Ask,
}

/// What a hook run means. Non-blocking errors and timeouts come back as `error`, and the
/// action proceeds.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    /// Why the action is blocked: exit 2, `decision: "block"`, or a `deny`.
    pub block: Option<String>,
    pub permission: Option<PermissionDecision>,
    pub permission_reason: Option<String>,
    pub updated_input: Option<Value>,
    pub error: Option<String>,
}

/// Interprets a finished hook. `None` for `code` means it was killed by a signal.
pub fn outcome(code: Option<i32>, stdout: &str, stderr: &str) -> Outcome {
    match code {
        Some(0) => {}
        Some(2) => {
            return Outcome {
                block: Some(stderr.trim().to_owned()),
                ..Outcome::default()
            };
        }
        _ => {
            return Outcome {
                error: Some(format!("hook failed ({code:?}): {}", stderr.trim())),
                ..Outcome::default()
            };
        }
    }
    let Ok(Value::Object(json)) = serde_json::from_str::<Value>(stdout.trim()) else {
        return Outcome::default();
    };
    let text = |map: &Map<String, Value>, key: &str| {
        map.get(key).and_then(Value::as_str).map(str::to_owned)
    };
    let specific = json
        .get("hookSpecificOutput")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let permission = match specific.get("permissionDecision").and_then(Value::as_str) {
        Some("allow") => Some(PermissionDecision::Allow),
        Some("deny") => Some(PermissionDecision::Deny),
        Some("ask") => Some(PermissionDecision::Ask),
        _ => None, // `defer` and anything unknown are no decision.
    };
    let permission_reason = text(&specific, "permissionDecisionReason");
    let block = if json.get("decision").and_then(Value::as_str) == Some("block") {
        Some(text(&json, "reason").unwrap_or_default())
    } else if permission == Some(PermissionDecision::Deny) {
        let why = permission_reason
            .clone()
            .or_else(|| text(&json, "systemMessage"));
        Some(why.unwrap_or_default())
    } else {
        None
    };
    Outcome {
        block,
        permission,
        permission_reason,
        updated_input: specific.get("updatedInput").cloned(),
        error: None,
    }
}

/// Runs `hook` with `input` on stdin. Only call this for hooks from approved plugins.
pub async fn run(hook: &Hook, input: &Value, plugin_root: &Path, project_dir: &Path) -> Outcome {
    let root = plugin_root.to_string_lossy();
    let expand = |s: &str| s.replace("${CLAUDE_PLUGIN_ROOT}", &root);
    let mut command = match &hook.args {
        Some(args) => {
            let mut c = tokio::process::Command::new(expand(&hook.command));
            c.args(args.iter().map(|a| expand(a)));
            c
        }
        None => {
            let mut c = tokio::process::Command::new("/bin/sh");
            c.arg("-c").arg(expand(&hook.command));
            c
        }
    };
    command
        .env_clear()
        .envs(child_env(std::env::vars_os()))
        .env("CLAUDE_PLUGIN_ROOT", plugin_root)
        .env("CLAUDE_PROJECT_DIR", project_dir)
        .current_dir(project_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let error = |e: String| Outcome {
        error: Some(e),
        ..Outcome::default()
    };
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return error(format!("couldn't start hook `{}`: {e}", hook.command)),
    };
    // The stdin write sits inside the timeout: a hook that never reads a large input
    // blocks the write forever. Dropping the future on timeout kills the child.
    let run_to_end = async move {
        if let Some(mut stdin) = child.stdin.take() {
            // A hook that ignores stdin closes it early; that isn't an error.
            let _ = stdin.write_all(input.to_string().as_bytes()).await;
        }
        child.wait_with_output().await
    };
    match tokio::time::timeout(hook.timeout, run_to_end).await {
        Ok(Ok(out)) => outcome(
            out.status.code(),
            &String::from_utf8_lossy(&out.stdout),
            &String::from_utf8_lossy(&out.stderr),
        ),
        Ok(Err(e)) => error(format!("hook `{}`: {e}", hook.command)),
        // Dropping the future kills the child (`kill_on_drop`).
        Err(_) => error(format!("hook `{}` timed out", hook.command)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matcher_forms() {
        let cases = [
            ("", "Bash", true),
            ("*", "Edit", true),
            ("Bash", "Bash", true),
            ("Bash", "BashX", false),
            ("Write|Edit", "Edit", true),
            ("Write, Edit", "Edit", true),
            ("Write|Edit", "Read", false),
            ("mcp__.*", "mcp__git__log", true),
            ("Ed.t", "Edit", true),
            ("^Re", "Read", true),
            ("^Re", "Edit", false),
        ];
        for (text, name, want) in cases {
            assert_eq!(
                Matcher::parse(text).unwrap().matches(name),
                want,
                "{text} {name}"
            );
        }
        assert!(
            Matcher::parse("(?<=x)y").is_none(),
            "lookaround doesn't compile"
        );
    }

    #[test]
    fn parse_skips_and_warns_on_unsupported_parts() {
        let (hooks, warnings) = parse(&json!({
            "PreToolUse": [{ "matcher": "Bash", "hooks": [
                { "type": "command", "command": "a", "args": ["x"], "timeout": 5 },
                { "type": "prompt", "prompt": "p" } ] },
                { "matcher": "(?!x)", "hooks": [{ "type": "command", "command": "b" }] }],
            "PostToolUse": [{ "matcher": "Write", "hooks": [{ "type": "command", "command": "c" }] }],
            "Notification": [],
        }));
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].args, Some(vec!["x".to_owned()]));
        assert_eq!(hooks[0].timeout, Duration::from_secs(5));
        assert_eq!(warnings.len(), 4, "{warnings:?}");
        assert!(
            warnings.contains(&"hooks: `PostToolUse` doesn't run yet, skipped".to_owned()),
            "{warnings:?}"
        );
    }

    #[test]
    fn exit_codes() {
        assert_eq!(outcome(Some(0), "", "").block, None);
        assert_eq!(outcome(Some(0), "hello\n", "").block, None);
        assert_eq!(outcome(Some(2), "", "no\n").block.as_deref(), Some("no"));
        // Exit 2 blocks even when the JSON allows.
        let allow = r#"{"hookSpecificOutput":{"permissionDecision":"allow"}}"#;
        assert_eq!(outcome(Some(2), allow, "why").block.as_deref(), Some("why"));
        for code in [Some(1), Some(127), None] {
            let o = outcome(code, allow, "boom");
            assert!(o.block.is_none() && o.permission.is_none() && o.error.is_some());
        }
    }

    #[test]
    fn output_fields() {
        let o = outcome(
            Some(0),
            &json!({
                "continue": false, "stopReason": "done", "systemMessage": "hi",
                "hookSpecificOutput": { "hookEventName": "PreToolUse",
                    "permissionDecision": "ask", "permissionDecisionReason": "check",
                    "updatedInput": { "a": 1 }, "additionalContext": "ctx",
                    "updatedToolOutput": "out" }
            })
            .to_string(),
            "",
        );
        assert_eq!(o.permission, Some(PermissionDecision::Ask));
        assert_eq!(o.permission_reason.as_deref(), Some("check"));
        assert_eq!(o.updated_input, Some(json!({ "a": 1 })));
        assert_eq!(o.block, None);

        let decision = |d: &str| {
            let out = json!({ "hookSpecificOutput": { "permissionDecision": d } }).to_string();
            outcome(Some(0), &out, "").permission
        };
        assert_eq!(decision("allow"), Some(PermissionDecision::Allow));
        assert_eq!(decision("defer"), None);
        let deny = r#"{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"r"}}"#;
        assert_eq!(outcome(Some(0), deny, "").block.as_deref(), Some("r"));
        let block = r#"{"decision":"block","reason":"r2"}"#;
        assert_eq!(outcome(Some(0), block, "").block.as_deref(), Some("r2"));
        // hookify denies with only a `systemMessage`; the model should still learn why.
        let hookify =
            r#"{"hookSpecificOutput":{"permissionDecision":"deny"},"systemMessage":"no rm"}"#;
        assert_eq!(
            outcome(Some(0), hookify, "").block.as_deref(),
            Some("no rm")
        );
    }

    fn hook(command: &str, args: Option<&[&str]>, secs: u64) -> Hook {
        Hook {
            event: "PreToolUse".to_owned(),
            matcher: Matcher::All,
            command: command.to_owned(),
            args: args.map(|a| a.iter().map(|s| (*s).to_owned()).collect()),
            timeout: Duration::from_secs(secs),
        }
    }

    #[tokio::test]
    async fn shell_and_exec_forms_get_stdin_and_root() {
        let dir = std::env::temp_dir();
        let shell = hook(
            "cat >&2; echo \" $CLAUDE_PLUGIN_ROOT\" >&2; exit 2",
            None,
            5,
        );
        let o = run(&shell, &json!({ "x": 1 }), Path::new("/plug"), &dir).await;
        assert_eq!(o.block.as_deref(), Some(r#"{"x":1} /plug"#));
        // Exec form: `;` is a literal argument, not a shell separator.
        let exec = hook(
            "/bin/sh",
            Some(&[
                "-c",
                "printf '%s' \"$1\" >&2; exit 2",
                "x",
                "${CLAUDE_PLUGIN_ROOT};",
            ]),
            5,
        );
        let o = run(&exec, &json!({}), Path::new("/plug"), &dir).await;
        assert_eq!(o.block.as_deref(), Some("/plug;"));
    }

    #[tokio::test]
    async fn a_timed_out_hook_is_a_non_blocking_error() {
        let slow = hook("exec sleep 30", None, 0);
        let o = tokio::time::timeout(
            Duration::from_secs(20),
            run(&slow, &json!({}), Path::new("/"), &std::env::temp_dir()),
        )
        .await
        .expect("run must honour the hook timeout, not hang");
        assert!(o.block.is_none());
        assert!(o.error.unwrap().contains("timed out"));
    }

    #[tokio::test]
    async fn a_hook_that_never_reads_big_stdin_still_times_out() {
        // Larger than the pipe buffer, so an unbounded stdin write would block forever.
        let big = json!({ "data": "x".repeat(1 << 20) });
        // The hook sleeps far longer than its 1s timeout, so an honoured timeout is well
        // below 30s even when a loaded machine delays spawning. `exec` makes sleep the
        // killed process rather than a shell that leaves it orphaned.
        let deaf = hook("exec sleep 30", None, 1);
        let started = std::time::Instant::now();
        // Outer guard: a hang fails the test instead of stalling the suite.
        let o = tokio::time::timeout(
            Duration::from_secs(20),
            run(&deaf, &big, Path::new("/"), &std::env::temp_dir()),
        )
        .await
        .expect("run must honour the hook timeout, not hang on stdin");
        assert!(o.error.unwrap().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
