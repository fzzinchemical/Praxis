//! The Landlock sandbox for `Bash` (SPEC §9). The praxis spawns its own binary as
//! `sandbox-exec --policy <json> -- <program> <args>`, which restricts itself and then execs the
//! program, so no `pre_exec` runs in the multi-threaded parent.
//!
// shortcut: Landlock doesn't restrict UDP or all socket types, so "no network" isn't absolute;
// add seccomp or a network namespace if this becomes a real threat.
// shortcut: Landlock is allowlist-only and can't exclude a subdirectory of a writable tree, so an
// approved Bash command can still write to .git/hooks or .praxis; run git with hooks disabled or
// mount those paths read-only in a namespace if this proves exploitable.

use landlock::{
    ABI, Access, AccessFs, AccessNet, RestrictionStatus, Ruleset, RulesetAttr, RulesetCreatedAttr,
    RulesetStatus, path_beneath_rules,
};
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// ABI 4 is the first that can block TCP, so it is the minimum.
const ABI_REQUIRED: ABI = ABI::V4;
/// Readable by every sandboxed command. Home folders are not, so `~/.ssh` stays out of reach.
const SYSTEM_READ: [&str; 9] = [
    "/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc", "/opt", "/proc", "/dev",
];
const DEVICES_WRITE: [&str; 4] = ["/dev/null", "/dev/zero", "/dev/full", "/dev/tty"];
/// Toolchain folders, read but never written: `~/.cargo/bin` runs later, unsandboxed.
const TOOLCHAIN_VARS: [&str; 6] = [
    "CARGO_HOME",
    "RUSTUP_HOME",
    "GOPATH",
    "GOROOT",
    "JAVA_HOME",
    "VIRTUAL_ENV",
];
const TOOLCHAIN_HOME_DIRS: [&str; 2] = [".cargo", ".rustup"];

/// Settings from config `[sandbox]`, which only user config and flags may set (SPEC §8).
#[derive(Debug, Clone, Default)]
pub struct Sandbox {
    /// A binary that understands `sandbox-exec`: the running `praxis`, or `xtask` in evals.
    pub helper: PathBuf,
    pub network: bool,
    pub extra_read: Vec<PathBuf>,
    pub extra_write: Vec<PathBuf>,
}

/// What `sandbox-exec` enforces. Paths that don't exist are skipped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub read: Vec<PathBuf>,
    pub write: Vec<PathBuf>,
    pub network: bool,
}

impl Policy {
    /// `env` is the scrubbed child environment, which names the home and toolchain folders.
    pub fn new(
        sandbox: &Sandbox,
        workspace: &Path,
        tmp: &Path,
        env: &[(OsString, OsString)],
    ) -> Self {
        let var = |name: &str| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| PathBuf::from(value))
        };
        let home = var("HOME");
        let mut read: Vec<PathBuf> = SYSTEM_READ.iter().map(PathBuf::from).collect();
        read.extend(TOOLCHAIN_VARS.iter().filter_map(|name| var(name)));
        read.extend(
            TOOLCHAIN_HOME_DIRS
                .iter()
                .filter_map(|dir| home.as_ref().map(|home| home.join(dir))),
        );
        // Git reads these on every call and warns when it can't, so expose only the config files.
        let xdg = var("XDG_CONFIG_HOME")
            .filter(|dir| !dir.as_os_str().is_empty())
            .or_else(|| home.as_ref().map(|home| home.join(".config")));
        read.extend(home.as_ref().map(|home| home.join(".gitconfig")));
        read.extend(xdg.map(|dir| dir.join("git/config")));
        read.extend(sandbox.extra_read.iter().cloned());
        let mut write = vec![workspace.to_owned(), tmp.to_owned()];
        write.extend(DEVICES_WRITE.iter().map(PathBuf::from));
        write.extend(sandbox.extra_write.iter().cloned());
        Self {
            read,
            write,
            network: sandbox.network,
        }
    }
}

/// Fail closed: anything short of full enforcement refuses to run (SPEC §9).
pub fn require_enforced(status: RulesetStatus, no_new_privs: bool) -> Result<(), String> {
    if status == RulesetStatus::FullyEnforced && no_new_privs {
        return Ok(());
    }
    Err(format!(
        "Commands can't run: the Landlock sandbox isn't fully enforced on this system \
         (status {status:?}; it needs Linux 6.7 or later with Landlock enabled). \
         To run commands without a sandbox, set `sandbox.mode = \"off\"` in \
         ~/.config/praxis/config.toml."
    ))
}

fn restrict(policy: &Policy) -> Result<RestrictionStatus, String> {
    let existing = |paths: &[PathBuf]| -> Vec<PathBuf> {
        paths.iter().filter(|p| p.exists()).cloned().collect()
    };
    let read = AccessFs::from_read(ABI_REQUIRED);
    let mut ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(ABI_REQUIRED))
        .map_err(|e| e.to_string())?;
    if !policy.network {
        // Handled with no port rules, so every TCP bind and connect is denied.
        ruleset = ruleset
            .handle_access(AccessNet::from_all(ABI_REQUIRED))
            .map_err(|e| e.to_string())?;
    }
    ruleset
        .create()
        .and_then(|r| r.add_rules(path_beneath_rules(existing(&policy.read), read)))
        .and_then(|r| {
            let all = AccessFs::from_all(ABI_REQUIRED);
            r.add_rules(path_beneath_rules(existing(&policy.write), all))
        })
        .and_then(|r| r.restrict_self())
        .map_err(|e| e.to_string())
}

/// The `sandbox-exec` entry point. `args` follow the subcommand: `--policy <json> -- <program>
/// [args]`. Returns only on failure, with the message to print; the caller exits non-zero.
pub fn exec(args: &[OsString]) -> String {
    let (policy, program) = match args {
        [flag, json, separator, program @ ..]
            if flag == "--policy" && separator == "--" && !program.is_empty() =>
        {
            (json, program)
        }
        _ => return "usage: sandbox-exec --policy <json> -- <program> [args]".to_owned(),
    };
    let policy: Policy = match serde_json::from_str(&policy.to_string_lossy()) {
        Ok(policy) => policy,
        Err(e) => return format!("sandbox-exec: invalid policy: {e}"),
    };
    let status = match restrict(&policy) {
        Ok(status) => status,
        Err(e) => return format!("sandbox-exec: {e}"),
    };
    if let Err(message) = require_enforced(status.ruleset, status.no_new_privs) {
        return message;
    }
    let error = Command::new(&program[0])
        .args(program[1..].iter().map(OsStr::new))
        .exec();
    format!(
        "sandbox-exec: can't run {}: {error}",
        program[0].to_string_lossy()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anything_short_of_full_enforcement_refuses_with_the_way_out() {
        assert_eq!(require_enforced(RulesetStatus::FullyEnforced, true), Ok(()));
        for (status, no_new_privs) in [
            (RulesetStatus::PartiallyEnforced, true),
            (RulesetStatus::NotEnforced, true),
            (RulesetStatus::FullyEnforced, false),
        ] {
            let message = require_enforced(status, no_new_privs).unwrap_err();
            assert!(message.contains("sandbox.mode = \"off\""), "{message}");
        }
    }

    #[test]
    fn the_policy_reads_toolchains_but_writes_only_the_workspace_and_temp() {
        let env = [
            (OsString::from("HOME"), OsString::from("/home/u")),
            (OsString::from("CARGO_HOME"), OsString::from("/opt/cargo")),
        ];
        let sandbox = Sandbox {
            extra_read: vec![PathBuf::from("/data")],
            ..Sandbox::default()
        };
        let policy = Policy::new(&sandbox, Path::new("/ws"), Path::new("/tmp/s"), &env);
        for path in ["/usr", "/opt/cargo", "/home/u/.cargo", "/data"] {
            assert!(policy.read.contains(&PathBuf::from(path)), "{path}");
        }
        assert!(policy.read.contains(&PathBuf::from("/home/u/.gitconfig")));
        assert!(
            policy
                .read
                .contains(&PathBuf::from("/home/u/.config/git/config"))
        );
        assert!(!policy.read.iter().any(|p| p.ends_with("git-credentials")));
        assert!(!policy.read.contains(&PathBuf::from("/home/u")));
        assert!(!policy.read.iter().any(|p| p.starts_with("/ws")));
        assert_eq!(
            policy.write[..2],
            [PathBuf::from("/ws"), PathBuf::from("/tmp/s")]
        );
        assert!(
            !policy
                .write
                .iter()
                .any(|p| p.starts_with("/home") || p.starts_with("/opt"))
        );
        assert!(!policy.network);
    }
}
