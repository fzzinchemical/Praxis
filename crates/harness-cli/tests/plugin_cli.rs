//! `harness plugin` against local git repos only (SPEC §8, §12).

// Lets clippy treat helper functions here as test code for `allow-unwrap-in-tests`.
#![cfg(test)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_harness");

fn temp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("harness-plugin-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        // Under the pre-commit hook these point at the harness repo itself.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// A git repo holding a plugin with a hook and an MCP server; returns its HEAD.
fn source_repo(dir: &Path) -> String {
    fs::create_dir_all(dir.join(".claude-plugin")).unwrap();
    fs::create_dir_all(dir.join("hooks")).unwrap();
    fs::write(
        dir.join(".claude-plugin/plugin.json"),
        r#"{"name": "demo"}"#,
    )
    .unwrap();
    fs::write(
        dir.join("hooks/hooks.json"),
        r#"{"hooks": {"PreToolUse": [{"matcher": "Bash",
            "hooks": [{"type": "command", "command": "echo hook-ran"}]}]}}"#,
    )
    .unwrap();
    fs::write(
        dir.join(".mcp.json"),
        r#"{"mcpServers": {"srv": {"command": "demo-server", "args": ["--stdio"]}}}"#,
    )
    .unwrap();
    git(dir, &["init", "--quiet"]);
    git(dir, &["add", "."]);
    git(dir, &["commit", "--quiet", "-m", "one"]);
    git(dir, &["rev-parse", "HEAD"])
}

fn harness(home: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .arg("plugin")
        .args(args)
        .current_dir(home)
        .env("XDG_CONFIG_HOME", home.join("xdg"))
        .env("XDG_DATA_HOME", home.join("xdg-data"))
        .env("HOME", home)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn install_pins_a_commit_and_lists_what_it_runs_before_enabling() {
    let home = temp("install");
    let src = home.join("src");
    fs::create_dir_all(&src).unwrap();
    let commit = source_repo(&src);

    let shown = stdout(&harness(&home, &["install", src.to_str().unwrap()]));
    for part in [
        commit.as_str(),
        "echo hook-ran",
        "PreToolUse",
        "demo-server --stdio",
        "disabled",
    ] {
        assert!(shown.contains(part), "missing {part}: {shown}");
    }

    // A later commit upstream doesn't change what is installed.
    fs::write(src.join("hooks/hooks.json"), r#"{"hooks": {}}"#).unwrap();
    git(&src, &["commit", "--quiet", "-am", "two"]);
    let listed = stdout(&harness(&home, &["list"]));
    assert!(
        listed.contains("demo") && listed.contains(&commit[..12]),
        "{listed}"
    );
    assert!(listed.contains("disabled"), "{listed}");

    let enabled = stdout(&harness(&home, &["enable", "demo"]));
    assert!(enabled.contains("echo hook-ran"), "{enabled}");
    assert!(stdout(&harness(&home, &["list"])).contains("enabled"));
    stdout(&harness(&home, &["disable", "demo"]));
    assert!(stdout(&harness(&home, &["list"])).contains("disabled"));
}

#[test]
fn installs_a_plugin_from_a_subdirectory_of_a_local_repo() {
    // Marketplace repos such as claude-plugins-official keep plugins in `plugins/<name>/`.
    let home = temp("subdir");
    let repo = home.join("market");
    let src = repo.join("plugins/demo");
    fs::create_dir_all(&src).unwrap();
    fs::write(repo.join("README.md"), "market").unwrap();
    let commit = source_repo(&src);
    fs::remove_dir_all(src.join(".git")).unwrap();
    git(&repo, &["init", "--quiet"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "--quiet", "-m", "market"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    assert_ne!(head, commit);

    let shown = stdout(&harness(&home, &["install", src.to_str().unwrap()]));
    assert!(
        shown.contains(&head) && shown.contains("echo hook-ran"),
        "{shown}"
    );
    let installed = home.join("xdg-data/harness/plugins/demo");
    assert!(installed.join("hooks/hooks.json").is_file());
    assert!(!installed.join("README.md").exists());
}

#[test]
fn remove_keeps_data_unless_its_removal_is_confirmed_separately() {
    let home = temp("remove");
    let src = home.join("src");
    fs::create_dir_all(&src).unwrap();
    source_repo(&src);
    let plugins = home.join("xdg-data/harness/plugins");
    let data = plugins.join("data/demo");

    stdout(&harness(&home, &["install", src.to_str().unwrap()]));
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("options.json"), "{}").unwrap();
    let removed = stdout(&harness(&home, &["remove", "demo"]));
    assert!(!plugins.join("demo").exists());
    assert!(data.exists(), "data deleted without confirmation");
    assert!(removed.contains("--delete-data"), "{removed}");
    assert!(!stdout(&harness(&home, &["list"])).contains("demo"));

    stdout(&harness(&home, &["install", src.to_str().unwrap()]));
    stdout(&harness(&home, &["remove", "demo", "--delete-data"]));
    assert!(!data.exists());
    assert!(!harness(&home, &["enable", "demo"]).status.success());
}

#[test]
fn list_json_reports_state_and_what_each_plugin_can_run() {
    let home = temp("list-json");
    let src = home.join("src");
    fs::create_dir_all(&src).unwrap();
    let commit = source_repo(&src);
    stdout(&harness(&home, &["install", src.to_str().unwrap()]));

    let listed: serde_json::Value =
        serde_json::from_str(&stdout(&harness(&home, &["list", "--json"]))).unwrap();
    let plugins = listed.as_array().expect("top level is an array");
    assert_eq!(plugins.len(), 1, "{listed}");
    let demo = &plugins[0];
    assert_eq!(demo["name"], "demo");
    assert_eq!(demo["commit"], commit.as_str());
    assert_eq!(demo["enabled"], false);
    assert!(demo["source"].as_str().unwrap().ends_with("src"), "{demo}");
    let can_run: Vec<&str> = demo["can_run"]
        .as_array()
        .expect("can_run is an array")
        .iter()
        .filter_map(|line| line.as_str())
        .collect();
    assert!(
        can_run.iter().any(|l| l.contains("echo hook-ran")),
        "{can_run:?}"
    );
    assert!(
        can_run.iter().any(|l| l.contains("demo-server --stdio")),
        "{can_run:?}"
    );

    stdout(&harness(&home, &["enable", "demo"]));
    let enabled: serde_json::Value =
        serde_json::from_str(&stdout(&harness(&home, &["list", "--json"]))).unwrap();
    assert_eq!(enabled[0]["enabled"], true);
}

#[test]
fn list_json_is_an_empty_array_when_nothing_is_installed() {
    let home = temp("list-json-empty");
    assert_eq!(stdout(&harness(&home, &["list", "--json"])).trim(), "[]");
}

#[test]
fn list_json_still_lists_a_plugin_whose_manifest_is_broken() {
    let home = temp("list-json-broken");
    let good = home.join("good");
    fs::create_dir_all(&good).unwrap();
    source_repo(&good);
    stdout(&harness(&home, &["install", good.to_str().unwrap()]));

    // A second plugin, named "other" by its manifest, so the broken one is not "demo".
    let bad = home.join("bad");
    fs::create_dir_all(&bad).unwrap();
    source_repo(&bad);
    fs::write(
        bad.join(".claude-plugin/plugin.json"),
        r#"{"name": "other"}"#,
    )
    .unwrap();
    git(&bad, &["commit", "--quiet", "-am", "rename"]);
    stdout(&harness(&home, &["install", bad.to_str().unwrap()]));

    // Break the installed copy's manifest so loading it fails.
    let installed = home.join("xdg-data/harness/plugins/other/.claude-plugin/plugin.json");
    fs::write(&installed, "{not json").unwrap();

    let out = harness(&home, &["list", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let listed: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    let plugins = listed.as_array().expect("top level is an array");
    assert_eq!(plugins.len(), 2, "{listed}");

    let demo = plugins
        .iter()
        .find(|p| p["name"] == "demo")
        .expect("demo listed");
    assert!(demo["error"].is_null(), "{demo}");
    assert!(!demo["can_run"].as_array().unwrap().is_empty(), "{demo}");

    let other = plugins
        .iter()
        .find(|p| p["name"] == "other")
        .expect("other listed");
    assert!(
        other["error"].as_str().is_some_and(|e| !e.is_empty()),
        "{other}"
    );
    assert_eq!(other["can_run"], serde_json::json!([]), "{other}");
}
