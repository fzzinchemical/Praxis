//! Per-session tool permissions in `ask` mode (SPEC §9).

use crate::agent::PermissionReply;
use std::collections::HashSet;

// shortcut: only `ask` mode's table, with session grants per tool; rules, hooks and the other
// modes come with the full permissions task in M3.
#[derive(Debug, Default)]
pub(crate) struct Permissions {
    allowed: HashSet<String>,
    rejected: HashSet<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Allow,
    Ask { can_always: bool },
    Deny(String),
}

impl Permissions {
    /// Closed by default: a tool that isn't listed here never runs.
    pub(crate) fn decide(&self, tool: &str) -> Decision {
        if self.rejected.contains(tool) {
            return Decision::Deny(rejected(tool));
        }
        match tool {
            "Read" | "Grep" | "Glob" => Decision::Allow,
            _ if self.allowed.contains(tool) => Decision::Allow,
            "Write" | "Edit" => Decision::Ask { can_always: true },
            // Unsandboxed until T3.3, so every call must prompt.
            "Bash" => Decision::Ask { can_always: false },
            other => Decision::Deny(format!(
                "Unknown tool `{other}`. Use one of the tools you were given."
            )),
        }
    }

    /// Stores any session grant and returns whether this call may run.
    pub(crate) fn record(&mut self, tool: &str, reply: PermissionReply) -> Result<(), String> {
        match reply {
            PermissionReply::AllowOnce => Ok(()),
            PermissionReply::AllowAlways => {
                if self.decide(tool) == (Decision::Ask { can_always: true }) {
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
}

fn rejected(tool: &str) -> String {
    format!("The user rejected this `{tool}` call, so it didn't run. Try another approach or ask.")
}
