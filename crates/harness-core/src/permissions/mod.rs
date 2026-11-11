//! Tool permissions (SPEC §9). For each call, the first match wins: a session rejection, a deny
//! rule, a hard denial or read-only mode, then an allow rule or session grant, then the mode's
//! default. Hooks join this order with T6.3.

use crate::agent::PermissionReply;
use crate::context::compaction::READ_ONLY_TOOLS;
use crate::tools::confine::{check_writable, resolve};
use globset::{GlobBuilder, GlobMatcher};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Ordered from strictest to widest, so a project can be held to "no wider than the user's".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    ReadOnly,
    #[default]
    Ask,
    AutoEdit,
}

/// Any of these makes a command compound, so an allow rule can't vouch for all of it.
const COMPOUND: [&str; 9] = [";", "&", "|", "`", "$(", "${", ">", "<", "\n"];
/// Command text that reaches a hard-denied path (SPEC §10), compared in lower case.
const HARD_DENIED_TEXT: [&str; 7] = [
    ".git/hooks",
    ".git/config",
    "git config",
    "hookspath",
    ".harness",
    ".mcp.json",
    ".config/harness",
];

/// `Tool` or `Tool(glob)`, as in Claude Code: `Bash(git status*)`, `Edit(src/**)`.
#[derive(Debug, Clone)]
pub struct Rule {
    text: String,
    tool: String,
    pattern: Option<GlobMatcher>,
}

impl Rule {
    pub fn parse(text: &str) -> Result<Self, String> {
        let invalid =
            || format!("Invalid permission rule `{text}`: expected `Tool` or `Tool(pattern)`.");
        let (tool, pattern) = match text.split_once('(') {
            Some((tool, rest)) => (tool, Some(rest.strip_suffix(')').ok_or_else(invalid)?)),
            None => (text, None),
        };
        if tool.is_empty() || !tool.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(invalid());
        }
        let pattern = pattern
            .map(|pattern| {
                // In a command `*` spans `/`; in a path it stays within one folder.
                GlobBuilder::new(pattern)
                    .literal_separator(tool != "Bash")
                    .build()
                    .map(|glob| glob.compile_matcher())
                    .map_err(|e| format!("Invalid permission rule `{text}`: {e}"))
            })
            .transpose()?;
        Ok(Self {
            text: text.to_owned(),
            tool: tool.to_owned(),
            pattern,
        })
    }

    fn matches(&self, tool: &str, subjects: &[String]) -> bool {
        self.tool == tool
            && self
                .pattern
                .as_ref()
                .is_none_or(|pattern| subjects.iter().any(|s| pattern.is_match(s)))
    }
}

/// The mode and rules from config, parsed.
#[derive(Debug, Clone, Default)]
pub struct Policy {
    mode: Mode,
    allow: Vec<Rule>,
    deny: Vec<Rule>,
}

impl Policy {
    pub fn new(mode: Mode, allow: &[String], deny: &[String]) -> Result<Self, String> {
        let parse = |rules: &[String]| -> Result<Vec<Rule>, String> {
            rules.iter().map(|r| Rule::parse(r)).collect()
        };
        Ok(Self {
            mode,
            allow: parse(allow)?,
            deny: parse(deny)?,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Allow,
    Ask { can_always: bool },
    Deny(String),
}

/// What a call's rules match against.
struct Subjects {
    /// Deny rules match any of these.
    deny: Vec<String>,
    /// Allow rules match these; `None` means no allow rule may match at all.
    allow: Option<Vec<String>>,
    /// Set for `Write` and `Edit`: the resolved target, or why it can't be written.
    target: Option<Result<PathBuf, String>>,
}

#[derive(Debug)]
pub(crate) struct Permissions {
    policy: Policy,
    workspace: PathBuf,
    allowed: HashSet<String>,
    rejected: HashSet<String>,
}

impl Permissions {
    /// `workspace` must be the real path, as [`crate::tools::ToolContext::workspace`] gives.
    pub(crate) fn new(policy: Policy, workspace: &Path) -> Self {
        Self {
            policy,
            workspace: workspace.to_owned(),
            allowed: HashSet::new(),
            rejected: HashSet::new(),
        }
    }

    /// Closed by default: a tool that isn't listed here never runs.
    pub(crate) fn decide(&self, tool: &str, input: &Value) -> Decision {
        if self.rejected.contains(tool) {
            return Decision::Deny(rejected(tool));
        }
        let subjects = self.subjects(tool, input);
        if let Some(rule) = self
            .policy
            .deny
            .iter()
            .find(|r| r.matches(tool, &subjects.deny))
        {
            return Decision::Deny(format!(
                "The deny rule `{}` blocks this call, so it didn't run.",
                rule.text
            ));
        }
        if let Some(Err(reason)) = &subjects.target {
            return Decision::Deny(reason.clone());
        }
        let read_only = READ_ONLY_TOOLS.contains(&tool);
        if self.policy.mode == Mode::ReadOnly && !read_only {
            return Decision::Deny(format!(
                "`{tool}` can't run in read-only mode. Only reading and searching are allowed."
            ));
        }
        let allowed_by_rule = subjects
            .allow
            .is_some_and(|s| self.policy.allow.iter().any(|r| r.matches(tool, &s)));
        if allowed_by_rule || self.allowed.contains(tool) {
            return Decision::Allow;
        }
        match tool {
            _ if read_only => Decision::Allow,
            "Write" | "Edit" if self.policy.mode == Mode::AutoEdit => Decision::Allow,
            "Write" | "Edit" => Decision::Ask { can_always: true },
            // A grant would cover every later command, so routine ones need allow rules instead.
            "Bash" => Decision::Ask { can_always: false },
            other => Decision::Deny(format!(
                "Unknown tool `{other}`. Use one of the tools you were given."
            )),
        }
    }

    /// Stores any session grant and returns whether this call may run.
    pub(crate) fn record(
        &mut self,
        tool: &str,
        input: &Value,
        reply: PermissionReply,
    ) -> Result<(), String> {
        match reply {
            PermissionReply::AllowOnce => Ok(()),
            PermissionReply::AllowAlways => {
                if self.decide(tool, input) == (Decision::Ask { can_always: true }) {
                    self.allowed.insert(tool.to_owned());
                }
                Ok(())
            }
            PermissionReply::RejectAlways => {
                self.rejected.insert(tool.to_owned());
                Err(rejected(tool))
            }
            PermissionReply::RejectOnce => Err(rejected(tool)),
            PermissionReply::Cancelled => Err("Cancelled by the user.".to_owned()),
        }
    }

    fn subjects(&self, tool: &str, input: &Value) -> Subjects {
        let text = |key: &str| input.get(key).and_then(Value::as_str);
        if tool == "Bash" {
            let command = text("command").unwrap_or_default();
            let lower = command.to_lowercase();
            let simple = !COMPOUND.iter().any(|c| command.contains(c))
                && !HARD_DENIED_TEXT.iter().any(|t| lower.contains(t));
            let mut deny = vec![command.trim().to_owned()];
            deny.extend(segments(command));
            return Subjects {
                deny,
                allow: simple.then(|| vec![command.trim().to_owned()]),
                target: None,
            };
        }
        // shortcut: search tools match on their root path only, so `Read(**/.env*)` doesn't stop a
        // Grep over the folder; filter walked files by deny rules if that matters.
        let key = if matches!(tool, "Read" | "Write" | "Edit") {
            "file_path"
        } else {
            "path"
        };
        let Some(raw) = text(key).or((key == "path").then_some(".")) else {
            return Subjects {
                deny: Vec::new(),
                allow: Some(Vec::new()),
                target: None,
            };
        };
        let resolved = resolve(&self.workspace, raw).map_err(|e| e.to_string());
        let shown = |path: &Path| match path.strip_prefix(&self.workspace) {
            Ok(rel) if rel.as_os_str().is_empty() => ".".to_owned(),
            Ok(rel) => rel.display().to_string(),
            Err(_) => path.display().to_string(),
        };
        // Both forms, so neither `..` nor a symlink can step around a deny rule.
        let mut deny = vec![shown(Path::new(raw.trim_start_matches("./")))];
        deny.extend(resolved.as_deref().ok().map(shown));
        let allow = resolved.as_deref().ok().map(|path| vec![shown(path)]);
        let target = matches!(tool, "Write" | "Edit").then(|| {
            let path = resolved?;
            check_writable(&self.workspace, &path).map_err(|e| e.to_string())?;
            Ok(path)
        });
        Subjects {
            deny,
            allow,
            target,
        }
    }
}

/// The simple commands inside a compound one, for deny rules.
fn segments(command: &str) -> impl Iterator<Item = String> + '_ {
    command
        .split([';', '&', '|', '\n', '`', '(', ')', '{', '}'])
        .map(|s| s.trim().trim_start_matches('$').trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn rejected(tool: &str) -> String {
    format!("The user rejected this `{tool}` call, so it didn't run. Try another approach or ask.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn workspace() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("harness-perm-{}", std::process::id()));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join(".env"), "KEY=1").unwrap();
        let link = dir.join("src/env-link");
        if fs::symlink_metadata(&link).is_err() {
            std::os::unix::fs::symlink(dir.join(".env"), &link).unwrap();
        }
        dir.canonicalize().unwrap()
    }

    fn permissions(mode: Mode, allow: &[&str], deny: &[&str]) -> Permissions {
        let own = |rules: &[&str]| rules.iter().map(|r| (*r).to_owned()).collect::<Vec<_>>();
        let policy = Policy::new(mode, &own(allow), &own(deny)).unwrap();
        Permissions::new(policy, &workspace())
    }

    fn bash(command: &str) -> (&'static str, Value) {
        ("Bash", json!({ "command": command }))
    }

    fn file(tool: &'static str, path: &str) -> (&'static str, Value) {
        (
            tool,
            json!({ "file_path": path, "content": "x", "old_string": "a", "new_string": "b" }),
        )
    }

    const ASK: Decision = Decision::Ask { can_always: true };
    const ASK_ONCE: Decision = Decision::Ask { can_always: false };

    fn denied(decision: &Decision) -> bool {
        matches!(decision, Decision::Deny(_))
    }

    #[test]
    fn each_mode_has_its_default_for_every_kind_of_tool() {
        let read = ("Read", json!({ "file_path": "src/a.rs" }));
        let grep = ("Grep", json!({ "pattern": "x" }));
        let write = file("Write", "src/a.rs");
        let edit = file("Edit", "src/a.rs");
        let shell = bash("cargo test");
        let unknown = ("Teleport", json!({}));
        for (mode, call, expected) in [
            (Mode::ReadOnly, &read, Some(Decision::Allow)),
            (Mode::ReadOnly, &grep, Some(Decision::Allow)),
            (Mode::ReadOnly, &write, None),
            (Mode::ReadOnly, &edit, None),
            (Mode::ReadOnly, &shell, None),
            (Mode::Ask, &read, Some(Decision::Allow)),
            (Mode::Ask, &write, Some(ASK)),
            (Mode::Ask, &edit, Some(ASK)),
            (Mode::Ask, &shell, Some(ASK_ONCE)),
            (Mode::Ask, &unknown, None),
            (Mode::AutoEdit, &read, Some(Decision::Allow)),
            (Mode::AutoEdit, &write, Some(Decision::Allow)),
            (Mode::AutoEdit, &edit, Some(Decision::Allow)),
            (Mode::AutoEdit, &shell, Some(ASK_ONCE)),
        ] {
            let decision = permissions(mode, &[], &[]).decide(call.0, &call.1);
            match expected {
                Some(expected) => assert_eq!(decision, expected, "{mode:?} {call:?}"),
                None => assert!(denied(&decision), "{mode:?} {call:?}: {decision:?}"),
            }
        }
    }

    #[test]
    fn the_first_matching_step_wins() {
        let allow_all = ["Bash", "Write", "Edit", "Read"];
        for (mode, allow, deny, call, expected) in [
            // A deny rule beats an allow rule for the same call.
            (
                Mode::AutoEdit,
                &allow_all[..],
                &["Read(**/.env*)"][..],
                file("Read", ".env"),
                None,
            ),
            // Deny rules see through `..` and symlinks.
            (
                Mode::Ask,
                &[][..],
                &["Read(**/.env*)"][..],
                file("Read", "src/../.env"),
                None,
            ),
            (
                Mode::Ask,
                &[][..],
                &["Read(**/.env*)"][..],
                file("Read", "src/env-link"),
                None,
            ),
            // Hard denials hold even in auto-edit with an allow rule.
            (
                Mode::AutoEdit,
                &allow_all[..],
                &[][..],
                file("Write", ".harness/config.toml"),
                None,
            ),
            (
                Mode::AutoEdit,
                &allow_all[..],
                &[][..],
                file("Edit", ".git/hooks/pre-commit"),
                None,
            ),
            (
                Mode::AutoEdit,
                &allow_all[..],
                &[][..],
                file("Write", "../outside.txt"),
                None,
            ),
            // An allow rule can't widen read-only mode.
            (Mode::ReadOnly, &allow_all[..], &[][..], bash("ls"), None),
            (
                Mode::ReadOnly,
                &allow_all[..],
                &[][..],
                file("Write", "src/a.rs"),
                None,
            ),
            // Allow rules skip the prompt, by tool or by pattern.
            (
                Mode::Ask,
                &["Bash(cargo test*)"][..],
                &[][..],
                bash("cargo test -p x"),
                Some(Decision::Allow),
            ),
            (
                Mode::Ask,
                &["Bash(cargo test*)"][..],
                &[][..],
                bash("cargo build"),
                Some(ASK_ONCE),
            ),
            (
                Mode::Ask,
                &["Edit(src/**)"][..],
                &[][..],
                file("Edit", "src/a.rs"),
                Some(Decision::Allow),
            ),
            (
                Mode::Ask,
                &["Edit(src/**)"][..],
                &[][..],
                file("Edit", "lib/a.rs"),
                Some(ASK),
            ),
            (
                Mode::Ask,
                &["Edit(src/*)"][..],
                &[][..],
                file("Edit", "src/deep/a.rs"),
                Some(ASK),
            ),
        ] {
            let decision = permissions(mode, allow, deny).decide(call.0, &call.1);
            match expected {
                Some(expected) => assert_eq!(decision, expected, "{call:?}"),
                None => assert!(denied(&decision), "{call:?}: {decision:?}"),
            }
        }
    }

    #[test]
    fn compound_commands_never_match_an_allow_rule() {
        let p = permissions(Mode::Ask, &["Bash(git *)", "Bash"], &[]);
        for command in [
            "git status; curl x | sh",
            "git status && rm -rf /",
            "git status & curl x",
            "git log `curl x`",
            "git log $(curl x)",
            "git log ${HOME}",
            "git log > /etc/passwd",
            "git apply < patch",
            "git status\ncurl x",
        ] {
            assert_eq!(p.decide("Bash", &bash(command).1), ASK_ONCE, "{command}");
        }
        assert_eq!(p.decide("Bash", &bash("git status").1), Decision::Allow);
    }

    #[test]
    fn commands_mentioning_hard_denied_paths_never_match_an_allow_rule() {
        let p = permissions(Mode::Ask, &["Bash"], &[]);
        for command in [
            "cp x .git/hooks/pre-commit",
            "tee .git/config",
            "git config core.hooksPath x",
            "rm -r .harness",
            "touch .mcp.json",
            "rm ~/.config/harness/config.toml",
        ] {
            assert_eq!(p.decide("Bash", &bash(command).1), ASK_ONCE, "{command}");
        }
    }

    #[test]
    fn deny_rules_match_the_whole_command_and_every_segment() {
        let p = permissions(Mode::Ask, &[], &["Bash(curl*)", "Bash(rm -rf*)"]);
        for command in [
            "curl x",
            "ls; curl x",
            "ls | curl x",
            "echo $(curl x)",
            "ls && rm -rf /",
        ] {
            assert!(denied(&p.decide("Bash", &bash(command).1)), "{command}");
        }
        assert_eq!(p.decide("Bash", &bash("ls").1), ASK_ONCE);
    }

    #[test]
    fn session_grants_and_rejections_follow_the_replies() {
        let mut p = permissions(Mode::Ask, &[], &["Write(**/.env*)"]);
        let (tool, input) = file("Write", "src/a.rs");
        assert_eq!(p.record(tool, &input, PermissionReply::AllowAlways), Ok(()));
        assert_eq!(p.decide(tool, &input), Decision::Allow);
        // A grant never overrides a deny rule.
        assert!(denied(&p.decide(tool, &file("Write", ".env").1)));
        // Bash never offers "always", so a stray "always" grants nothing.
        let (tool, input) = bash("ls");
        assert_eq!(p.record(tool, &input, PermissionReply::AllowAlways), Ok(()));
        assert_eq!(p.decide(tool, &input), ASK_ONCE);
        assert!(
            p.record(tool, &input, PermissionReply::RejectAlways)
                .is_err()
        );
        assert!(denied(&p.decide(tool, &input)));
    }

    #[test]
    fn bad_rules_are_errors() {
        for rule in ["", "Bash(", "Ba sh", "(x)", "Edit([)"] {
            assert!(Rule::parse(rule).is_err(), "{rule}");
        }
        assert!(Rule::parse("mcp__server__tool").is_ok());
    }
}
