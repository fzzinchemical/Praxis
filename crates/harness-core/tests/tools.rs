//! Built-in tool behaviour through the public interface (SPEC §6, §9, §10).

// Lets clippy treat helper functions here as test code for `allow-unwrap-in-tests`.
#![cfg(test)]

use harness_core::tools::{ToolContext, ToolOutput, execute, schemas};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio_util::sync::CancellationToken;

struct Fixture {
    root: PathBuf,
    ctx: Arc<ToolContext>,
}

impl Fixture {
    /// `<root>/ws` is the workspace, so `<root>/outside.txt` is a real file outside it.
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("harness-tools-{}-{id}", std::process::id()));
        fs::create_dir_all(root.join("ws")).unwrap();
        fs::write(root.join("outside.txt"), "secret\n").unwrap();
        let ctx = Arc::new(ToolContext::new(&root.join("ws")).unwrap());
        Self { root, ctx }
    }

    fn ws(&self) -> PathBuf {
        self.root.join("ws")
    }

    fn file(&self, name: &str, content: &str) -> PathBuf {
        let path = self.ws().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        path
    }

    async fn call(&self, name: &str, input: Value) -> ToolOutput {
        execute(name, input, self.ctx.clone(), CancellationToken::new()).await
    }

    async fn write(&self, path: &str, content: &str) -> ToolOutput {
        self.call("Write", json!({ "file_path": path, "content": content }))
            .await
    }

    async fn read(&self, path: &str) -> ToolOutput {
        self.call("Read", json!({ "file_path": path })).await
    }

    async fn edit(&self, path: &str, old: &str, new: &str) -> ToolOutput {
        self.call(
            "Edit",
            json!({ "file_path": path, "old_string": old, "new_string": new }),
        )
        .await
    }

    fn outside_unchanged(&self) {
        assert_eq!(
            fs::read_to_string(self.root.join("outside.txt")).unwrap(),
            "secret\n"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn err(output: &ToolOutput, needle: &str) {
    assert!(output.is_error, "expected an error: {output:?}");
    assert!(
        output.content.contains(needle),
        "{needle:?} not in {output:?}"
    );
}

fn ok(output: &ToolOutput, needle: &str) {
    assert!(!output.is_error, "unexpected error: {output:?}");
    assert!(
        output.content.contains(needle),
        "{needle:?} not in {output:?}"
    );
}

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn schemas_keep_the_claude_code_names_and_parameters() {
    let tools = schemas();
    let found: Vec<(&str, Vec<&str>)> = tools
        .iter()
        .map(|t| {
            let props = t.parameters["properties"].as_object().unwrap();
            // Workspace feature unification can turn on serde_json's `preserve_order`.
            let mut names: Vec<&str> = props.keys().map(String::as_str).collect();
            names.sort_unstable();
            (t.name.as_str(), names)
        })
        .collect();
    assert_eq!(
        found,
        [
            ("Read", vec!["file_path", "limit", "offset"]),
            ("Write", vec!["content", "file_path"]),
            (
                "Edit",
                vec!["file_path", "new_string", "old_string", "replace_all"]
            ),
            ("Bash", vec!["command", "description", "timeout"]),
            (
                "Grep",
                vec!["-i", "glob", "multiline", "output_mode", "path", "pattern"]
            ),
            ("Glob", vec!["path", "pattern"]),
        ]
    );
    assert_eq!(tools[0].parameters["required"], json!(["file_path"]));
}

#[tokio::test]
async fn unknown_tools_and_bad_arguments_are_errors_for_the_model() {
    let f = Fixture::new();
    err(&f.call("Nope", json!({})).await, "Unknown tool");
    err(&f.call("Read", json!({})).await, "file_path");
    err(
        &f.call("Read", json!({ "file_path": "a", "x": 1 })).await,
        "x",
    );
}

#[tokio::test]
async fn read_returns_numbered_lines_within_the_requested_range() {
    let f = Fixture::new();
    f.file("a.txt", "one\ntwo\nthree\n");
    let all = f.read("a.txt").await;
    assert_eq!(all.content, "     1\tone\n     2\ttwo\n     3\tthree\n");
    let part = f
        .call(
            "Read",
            json!({ "file_path": "a.txt", "offset": 2, "limit": 1 }),
        )
        .await;
    ok(&part, "     2\ttwo\n");
    ok(&part, "lines 2-2 of 3");
    assert!(!part.content.contains("three"));
}

#[tokio::test]
async fn read_applies_a_default_line_limit() {
    let f = Fixture::new();
    f.file("long.txt", &"x\n".repeat(2500));
    let output = f.read("long.txt").await;
    ok(&output, "lines 1-2000 of 2500");
    assert!(!output.content.contains("2001\t"));
}

#[tokio::test]
async fn read_refuses_missing_binary_and_non_regular_files() {
    let f = Fixture::new();
    fs::write(f.ws().join("bin"), [0_u8, 159, 146]).unwrap();
    fs::create_dir(f.ws().join("dir")).unwrap();
    err(&f.read("missing.txt").await, "Can't read");
    err(&f.read("bin").await, "binary");
    err(&f.read("dir").await, "not a regular file");
}

#[tokio::test]
async fn write_creates_folders_and_keeps_its_result_short() {
    let f = Fixture::new();
    let output = f.write("new/dir/a.txt", "SENTINEL\nline\n").await;
    ok(&output, "2 lines");
    assert!(!output.content.contains("SENTINEL"));
    assert_eq!(output.structured["type"], "create");
    let path = f.ws().join("new/dir/a.txt").canonicalize().unwrap();
    assert_eq!(output.structured["filePath"], s(&path));
    assert_eq!(fs::read_to_string(&path).unwrap(), "SENTINEL\nline\n");
    assert_eq!(
        f.write("new/dir/a.txt", "x").await.structured["type"],
        "update"
    );
}

#[tokio::test]
async fn write_and_edit_preserve_the_file_mode() {
    let f = Fixture::new();
    let path = f.file("run.sh", "old\n");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
    f.write("run.sh", "older\n").await;
    f.read("run.sh").await;
    ok(&f.edit("run.sh", "older", "new").await, "1 replacement");
    assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
    let mode = fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o750);
}

#[tokio::test]
async fn write_refuses_a_directory() {
    let f = Fixture::new();
    fs::create_dir(f.ws().join("dir")).unwrap();
    err(&f.write("dir", "x").await, "not a regular file");
}

#[tokio::test]
async fn edit_requires_a_prior_read() {
    let f = Fixture::new();
    let path = f.file("a.txt", "hello\n");
    err(&f.edit("a.txt", "hello", "bye").await, "before editing");
    assert_eq!(fs::read_to_string(path).unwrap(), "hello\n");
}

#[tokio::test]
async fn edit_refuses_a_file_changed_since_it_was_read() {
    let f = Fixture::new();
    let path = f.file("a.txt", "hello\n");
    f.read("a.txt").await;
    fs::write(&path, "hello from a developer\n").unwrap();
    err(&f.edit("a.txt", "hello", "bye").await, "changed since");
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        "hello from a developer\n"
    );
}

#[tokio::test]
async fn edit_allows_consecutive_edits_after_one_read() {
    let f = Fixture::new();
    let path = f.file("a.txt", "one two\n");
    f.read("a.txt").await;
    ok(&f.edit("a.txt", "one", "1").await, "Edited");
    ok(&f.edit("a.txt", "two", "2").await, "Edited");
    assert_eq!(fs::read_to_string(path).unwrap(), "1 2\n");
}

#[tokio::test]
async fn edit_reports_a_missing_string_with_a_near_match() {
    let f = Fixture::new();
    f.file("a.rs", "fn main() {\n    run();\n}\n");
    f.read("a.rs").await;
    let output = f.edit("a.rs", "  run();\n  stop();", "x").await;
    err(&output, "not found");
    err(&output, "Line 2 is close");
    err(&f.edit("a.rs", "absent", "x").await, "not found");
}

#[tokio::test]
async fn edit_requires_a_unique_match_unless_replace_all() {
    let f = Fixture::new();
    let path = f.file("a.txt", "x x\n");
    f.read("a.txt").await;
    err(&f.edit("a.txt", "x", "y").await, "appears 2 times");
    let all = f
        .call(
            "Edit",
            json!({ "file_path": "a.txt", "old_string": "x", "new_string": "SENT", "replace_all": true }),
        )
        .await;
    ok(&all, "2 replacement");
    assert!(!all.content.contains("SENT"));
    assert_eq!(all.structured["newString"], "SENT");
    assert_eq!(fs::read_to_string(path).unwrap(), "SENT SENT\n");
}

#[tokio::test]
async fn edit_rejects_empty_and_identical_strings() {
    let f = Fixture::new();
    f.file("a.txt", "x\n");
    f.read("a.txt").await;
    err(&f.edit("a.txt", "", "y").await, "must not be empty");
    err(&f.edit("a.txt", "x", "x").await, "must differ");
}

#[tokio::test]
async fn dot_dot_cannot_escape_the_workspace() {
    let f = Fixture::new();
    err(&f.read("../outside.txt").await, "outside the workspace");
    err(
        &f.write("../outside.txt", "x").await,
        "outside the workspace",
    );
    err(&f.write("../new.txt", "x").await, "outside the workspace");
    err(
        &f.write("missing/../../new.txt", "x").await,
        "can't be resolved",
    );
    let absolute = f.root.join("outside.txt");
    err(&f.read(s(&absolute)).await, "outside the workspace");
    err(&f.write(s(&absolute), "x").await, "outside the workspace");
    f.outside_unchanged();
    assert!(!f.root.join("new.txt").exists());
}

#[tokio::test]
async fn a_sibling_with_the_workspace_as_name_prefix_is_outside() {
    let f = Fixture::new();
    fs::create_dir(f.root.join("ws2")).unwrap();
    err(&f.write("../ws2/a.txt", "x").await, "outside the workspace");
}

#[tokio::test]
async fn symlinks_cannot_escape_the_workspace() {
    let f = Fixture::new();
    symlink(f.root.join("outside.txt"), f.ws().join("file_link")).unwrap();
    symlink(&f.root, f.ws().join("dir_link")).unwrap();
    symlink(f.root.join("planted.txt"), f.ws().join("dangling")).unwrap();

    err(&f.read("file_link").await, "outside the workspace");
    err(
        &f.read("dir_link/outside.txt").await,
        "outside the workspace",
    );
    err(&f.write("file_link", "x").await, "outside the workspace");
    err(
        &f.write("dir_link/outside.txt", "x").await,
        "outside the workspace",
    );
    err(
        &f.write("dir_link/planted.txt", "x").await,
        "outside the workspace",
    );
    err(
        &f.edit("file_link", "secret", "x").await,
        "outside the workspace",
    );
    assert!(f.write("dangling", "x").await.is_error);
    assert!(f.write("dangling/below.txt", "x").await.is_error);
    f.outside_unchanged();
    assert!(!f.root.join("planted.txt").exists());
}

#[tokio::test]
async fn a_symlink_inside_the_workspace_writes_through_to_its_target() {
    let f = Fixture::new();
    let target = f.file("real.txt", "old");
    symlink(&target, f.ws().join("link")).unwrap();
    ok(&f.write("link", "new").await, "real.txt");
    assert_eq!(fs::read_to_string(target).unwrap(), "new");
    assert!(
        fs::symlink_metadata(f.ws().join("link"))
            .unwrap()
            .is_symlink()
    );
}

#[tokio::test]
async fn hard_denied_paths_refuse_write_and_edit_even_through_symlinks() {
    let f = Fixture::new();
    for path in [
        ".git/hooks/pre-commit",
        ".git/config",
        ".harness/x.toml",
        ".mcp.json",
    ] {
        f.file(path, "keep\n");
        f.read(path).await;
        err(&f.write(path, "x").await, "protected");
        err(&f.edit(path, "keep", "x").await, "protected");
        assert_eq!(fs::read_to_string(f.ws().join(path)).unwrap(), "keep\n");
    }
    err(&f.write(".git/hooks/new", "x").await, "protected");
    err(&f.write("sub/.git/hooks/new", "x").await, "protected");
    symlink(f.ws().join(".git/hooks"), f.ws().join("innocent")).unwrap();
    err(&f.write("innocent/new", "x").await, "protected");
    assert!(!f.ws().join(".git/hooks/new").exists());
    ok(&f.write(".github/ci.yml", "x").await, "Wrote");
}

#[tokio::test]
async fn grep_supports_each_output_mode() {
    let f = Fixture::new();
    f.file("a.rs", "alpha\nbeta alpha\n");
    f.file("b.txt", "gamma\n");
    let call = |mode: &str| json!({ "pattern": "alpha", "output_mode": mode });
    let files = f.call("Grep", json!({ "pattern": "alpha" })).await;
    assert_eq!(files.content, "a.rs");
    assert_eq!(f.call("Grep", call("count")).await.content, "a.rs:2");
    assert_eq!(
        f.call("Grep", call("content")).await.content,
        "a.rs:1:alpha\na.rs:2:beta alpha"
    );
    ok(
        &f.call("Grep", json!({ "pattern": "nothing" })).await,
        "No matches",
    );
}

#[tokio::test]
async fn grep_honours_case_multiline_glob_path_and_ignore_rules() {
    let f = Fixture::new();
    f.file("a.rs", "Hello\nWorld\n");
    f.file("sub/b.txt", "hello\n");
    f.file("skipped.log", "hello\n");
    f.file(".gitignore", "*.log\n");
    let grep = async |input: Value| f.call("Grep", input).await.content;
    assert_eq!(grep(json!({ "pattern": "hello" })).await, "sub/b.txt");
    let both = grep(json!({ "pattern": "hello", "-i": true })).await;
    assert!(
        both.contains("a.rs") && both.contains("sub/b.txt"),
        "{both}"
    );
    assert!(!both.contains("skipped"), "{both}");
    assert_eq!(
        grep(json!({ "pattern": "hello", "-i": true, "glob": "*.rs" })).await,
        "a.rs"
    );
    assert_eq!(
        grep(json!({ "pattern": "hello", "path": "sub" })).await,
        "sub/b.txt"
    );
    assert_eq!(
        grep(json!({ "pattern": "Hello.World", "multiline": true })).await,
        "a.rs"
    );
    assert_eq!(
        grep(json!({ "pattern": "Hello.World" })).await,
        "No matches."
    );
}

#[tokio::test]
async fn grep_and_glob_report_bad_input_and_skip_binary_files() {
    let f = Fixture::new();
    fs::write(f.ws().join("bin"), b"hello\0hello\n").unwrap();
    err(
        &f.call("Grep", json!({ "pattern": "(" })).await,
        "Invalid pattern",
    );
    err(
        &f.call("Glob", json!({ "pattern": "a[" })).await,
        "Invalid pattern",
    );
    err(
        &f.call("Grep", json!({ "pattern": "x", "path": "gone" }))
            .await,
        "Can't search",
    );
    ok(
        &f.call("Grep", json!({ "pattern": "hello" })).await,
        "No matches",
    );
}

#[tokio::test]
async fn searches_cannot_leave_the_workspace() {
    let f = Fixture::new();
    symlink(f.root.join("outside.txt"), f.ws().join("file_link")).unwrap();
    symlink(&f.root, f.ws().join("dir_link")).unwrap();
    let grep = |path: &str| json!({ "pattern": "secret", "path": path });
    let glob = |path: &str| json!({ "pattern": "**", "path": path });
    err(&f.call("Grep", grep("..")).await, "outside the workspace");
    err(
        &f.call("Grep", grep("dir_link")).await,
        "outside the workspace",
    );
    err(&f.call("Glob", glob("..")).await, "outside the workspace");
    ok(
        &f.call("Grep", json!({ "pattern": "secret" })).await,
        "No matches",
    );
    ok(
        &f.call("Glob", json!({ "pattern": "**/*.txt" })).await,
        "No matches",
    );
}

#[tokio::test]
async fn glob_matches_by_path_with_sorted_output() {
    let f = Fixture::new();
    f.file("b.rs", "");
    f.file("a.rs", "");
    f.file("sub/c.rs", "");
    f.file("sub/d.txt", "");
    let glob = async |input: Value| f.call("Glob", input).await.content;
    assert_eq!(glob(json!({ "pattern": "*.rs" })).await, "a.rs\nb.rs");
    assert_eq!(
        glob(json!({ "pattern": "**/*.rs" })).await,
        "a.rs\nb.rs\nsub/c.rs"
    );
    assert_eq!(
        glob(json!({ "pattern": "*.txt", "path": "sub" })).await,
        "sub/d.txt"
    );
}

#[tokio::test]
async fn cancelled_searches_stop() {
    let f = Fixture::new();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let output = execute("Grep", json!({ "pattern": "x" }), f.ctx.clone(), cancel).await;
    err(&output, "Cancelled");
}

fn bash(command: &str, timeout_ms: u64) -> Value {
    json!({ "command": command, "timeout": timeout_ms })
}

/// A zombie counts as dead: nothing reaps it until init gets around to it.
fn is_alive(pid: &str) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|stat| {
            let after_name = stat.rsplit(')').next().unwrap_or("");
            !after_name.trim_start().starts_with('Z')
        })
        .unwrap_or(false)
}

async fn assert_dies(pid: &str) {
    for _ in 0..100 {
        if !is_alive(pid) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("process {pid} is still alive");
}

const BACKGROUND_SLEEP: &str = "sleep 300 & echo $! > pid; wait";

#[tokio::test]
async fn bash_returns_output_exit_code_and_runs_in_the_workspace() {
    let f = Fixture::new();
    f.file("marker.txt", "");
    let output = f.call("Bash", bash("ls; echo oops >&2", 10_000)).await;
    ok(&output, "marker.txt");
    ok(&output, "oops");
    assert_eq!(output.structured["stdout"], "marker.txt\n");
    assert_eq!(output.structured["stderr"], "oops\n");
    assert_eq!(output.structured["interrupted"], false);
    assert_eq!(output.structured["isImage"], false);

    let failed = f.call("Bash", bash("exit 3", 10_000)).await;
    err(&failed, "exit code 3");
    err(&f.call("Bash", json!({})).await, "command");
}

#[tokio::test]
async fn bash_timeout_kills_a_spawned_grandchild() {
    let f = Fixture::new();
    let output = f.call("Bash", bash(BACKGROUND_SLEEP, 1000)).await;
    err(&output, "Timed out");
    assert_eq!(output.structured["interrupted"], true);
    let pid = fs::read_to_string(f.ws().join("pid")).unwrap();
    assert_dies(pid.trim()).await;
}

#[tokio::test]
async fn bash_cancel_kills_the_whole_process_group() {
    let f = Fixture::new();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let pid_file = f.ws().join("pid");
    tokio::spawn(async move {
        while !pid_file.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        trigger.cancel();
    });
    let output = execute(
        "Bash",
        bash(BACKGROUND_SLEEP, 60_000),
        f.ctx.clone(),
        cancel,
    )
    .await;
    err(&output, "Cancelled");
    assert_eq!(output.structured["interrupted"], true);
    let pid = fs::read_to_string(f.ws().join("pid")).unwrap();
    assert_dies(pid.trim()).await;
}

#[tokio::test]
async fn bash_returns_when_the_shell_exits_and_leaves_no_background_process() {
    let f = Fixture::new();
    let started = std::time::Instant::now();
    let output = f
        .call("Bash", bash("sleep 300 & echo $! > pid; echo done", 60_000))
        .await;
    ok(&output, "done");
    assert!(
        started.elapsed().as_secs() < 30,
        "waited for the background job"
    );
    let pid = fs::read_to_string(f.ws().join("pid")).unwrap();
    assert_dies(pid.trim()).await;
}

#[tokio::test]
async fn bash_child_environment_holds_only_allowlisted_variables() {
    let f = Fixture::new();
    let path = std::env::var_os("PATH").unwrap();
    let vars = [
        ("PATH", path.to_str().unwrap()),
        ("HOME", "/home/test"),
        ("GITHUB_TOKEN", "leak-1"),
        ("OPENAI_API_KEY", "leak-2"),
        ("AWS_SECRET", "leak-3"),
        ("DATABASE_PASSWORD", "leak-4"),
        ("github_token", "leak-5"),
    ]
    .map(|(k, v)| (k.into(), v.into()));
    let ctx = Arc::new(ToolContext::with_env(&f.ws(), vars).unwrap());
    let output = execute("Bash", bash("env", 10_000), ctx, CancellationToken::new()).await;
    ok(&output, "HOME=/home/test");
    assert!(!output.content.contains("leak"), "{}", output.content);
    assert!(!output.content.contains("TOKEN"), "{}", output.content);
}

#[tokio::test]
async fn bash_caps_captured_output_and_says_so() {
    let f = Fixture::new();
    let output = f
        .call(
            "Bash",
            bash("head -c 3000000 /dev/zero | tr '\\0' x", 30_000),
        )
        .await;
    ok(&output, "output truncated");
    assert!(output.content.len() < 1_100_000);
}
