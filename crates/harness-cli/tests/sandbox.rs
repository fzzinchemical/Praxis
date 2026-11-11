//! The Landlock sandbox on the real kernel, through the hidden `sandbox-exec` (SPEC §9).

// Lets clippy treat helper functions here as test code for `allow-unwrap-in-tests`.
#![cfg(test)]

use harness_core::sandbox::{Policy, Sandbox};
use std::ffi::OsString;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

/// `<root>/ws` is the workspace, `<root>/home` a fake home with an SSH key, `<root>/outside` a
/// folder nobody granted.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("harness-sandbox-{}-{id}", std::process::id()));
        for dir in ["ws", "home/.ssh", "outside", "tmp"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(root.join("home/.ssh/id_ed25519"), "PRIVATE KEY").unwrap();
        Self {
            root: root.canonicalize().unwrap(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn run(&self, network: bool, script: &str) -> Output {
        let env = [
            (OsString::from("HOME"), self.path("home").into_os_string()),
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        ];
        let sandbox = Sandbox {
            helper: PathBuf::from(BIN),
            network,
            ..Sandbox::default()
        };
        let policy = Policy::new(&sandbox, &self.path("ws"), &self.path("tmp"), &env);
        Command::new(BIN)
            .args([
                "sandbox-exec",
                "--policy",
                &serde_json::to_string(&policy).unwrap(),
                "--",
            ])
            .args(["/bin/sh", "-c", script])
            .current_dir(self.path("ws"))
            .env_clear()
            .envs(env)
            .output()
            .unwrap()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn exists(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

#[test]
fn commands_write_in_the_workspace_and_temp_but_nowhere_else() {
    let f = Fixture::new();
    let script = format!(
        "echo hi > made.txt && echo hi > '{}/x'",
        f.path("tmp").display()
    );
    let inside = f.run(false, &script);
    assert!(inside.status.success(), "{}", stderr(&inside));
    assert!(exists(&f.path("ws/made.txt")));

    let outside = f.path("outside/escaped.txt");
    let output = f.run(false, &format!("echo x > '{}'", outside.display()));
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("Permission denied"),
        "{}",
        stderr(&output)
    );
    assert!(!exists(&outside));

    let home = f.path("home/.bashrc");
    let output = f.run(false, &format!("echo x > '{}'", home.display()));
    assert!(!output.status.success());
    assert!(!exists(&home));
}

#[test]
fn commands_cannot_read_ssh_keys_in_home() {
    let f = Fixture::new();
    let output = f.run(false, "cat \"$HOME/.ssh/id_ed25519\"");
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("PRIVATE KEY"));
    assert!(
        stderr(&output).contains("Permission denied"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn tcp_connections_are_blocked_unless_network_is_allowed() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // bash, since /dev/tcp is a bash feature and /bin/sh may be another shell.
    let connect = format!("exec bash -c 'exec 3<>/dev/tcp/127.0.0.1/{port}'");
    let f = Fixture::new();

    let blocked = f.run(false, &connect);
    assert!(!blocked.status.success());
    assert!(
        stderr(&blocked).contains("Permission denied"),
        "{}",
        stderr(&blocked)
    );

    let allowed = f.run(true, &connect);
    assert!(allowed.status.success(), "{}", stderr(&allowed));
}

#[test]
fn a_bad_invocation_never_runs_the_command() {
    let f = Fixture::new();
    let marker = f.path("ws/ran");
    let output = Command::new(BIN)
        .args([
            "sandbox-exec",
            "--policy",
            "{not json",
            "--",
            "/bin/sh",
            "-c",
        ])
        .arg(format!("touch '{}'", marker.display()))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(126));
    assert!(!exists(&marker));
}
