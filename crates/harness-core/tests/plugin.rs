#![cfg(test)]

use harness_core::plugin::{Source, load};
use std::fs;
use std::path::{Path, PathBuf};

fn plugin_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("harness-plugin-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn manifest(root: &Path, json: &str) {
    write(&root.join(".claude-plugin/plugin.json"), json);
}

#[test]
fn a_plugin_without_a_manifest_loads_its_default_components() {
    let root = plugin_dir("bare");
    write(&root.join("skills/review/SKILL.md"), "body");
    write(&root.join("commands/fix.md"), "body");
    write(&root.join("agents/team/writer.md"), "body");
    write(&root.join("hooks/hooks.json"), "{}");
    write(&root.join(".mcp.json"), "{}");
    let plugin = load(&root).unwrap();
    assert!(plugin.name.starts_with("harness-plugin-"));
    assert_eq!(plugin.skills, [root.join("skills")]);
    assert_eq!(plugin.commands, [root.join("commands/fix.md")]);
    assert_eq!(plugin.agents, [root.join("agents/team/writer.md")]);
    assert_eq!(plugin.hooks, [Source::File(root.join("hooks/hooks.json"))]);
    assert_eq!(plugin.mcp_servers, [Source::File(root.join(".mcp.json"))]);
    assert!(plugin.warnings.is_empty(), "{:?}", plugin.warnings);
}

#[test]
fn commands_and_agents_replace_the_default_scan() {
    let root = plugin_dir("replace");
    write(&root.join("commands/default.md"), "");
    write(&root.join("agents/default.md"), "");
    write(&root.join("extra/cmd.md"), "");
    write(&root.join("crew/a.md"), "");
    manifest(
        &root,
        r#"{"name":"p","commands":"./extra/cmd.md","agents":["./crew"]}"#,
    );
    let plugin = load(&root).unwrap();
    assert_eq!(plugin.name, "p");
    assert_eq!(plugin.commands, [root.join("extra/cmd.md")]);
    assert_eq!(plugin.agents, [root.join("crew/a.md")]);
}

#[test]
fn skills_add_to_the_default_scan_and_may_be_the_root() {
    let root = plugin_dir("add");
    write(&root.join("skills/a/SKILL.md"), "");
    write(&root.join("SKILL.md"), "");
    manifest(&root, r#"{"name":"p","skills":"."}"#);
    let plugin = load(&root).unwrap();
    assert_eq!(plugin.skills, [root.join("skills"), root.clone()]);
}

#[test]
fn hooks_and_mcp_servers_merge_with_their_default_files() {
    let root = plugin_dir("merge");
    write(&root.join("hooks/hooks.json"), "{}");
    write(&root.join(".mcp.json"), "{}");
    write(&root.join("more-hooks.json"), "{}");
    manifest(
        &root,
        r#"{"name":"p","hooks":["./more-hooks.json",{"PreToolUse":[]}],"mcpServers":{"db":{"command":"x"}}}"#,
    );
    let plugin = load(&root).unwrap();
    assert_eq!(
        plugin.hooks,
        [
            Source::File(root.join("hooks/hooks.json")),
            Source::File(root.join("more-hooks.json")),
            Source::Inline(serde_json::json!({"PreToolUse": []})),
        ]
    );
    assert_eq!(
        plugin.mcp_servers,
        [
            Source::File(root.join(".mcp.json")),
            Source::Inline(serde_json::json!({"db": {"command": "x"}})),
        ]
    );
}

#[test]
fn paths_must_start_with_dot_slash_exist_and_stay_inside_the_root() {
    let root = plugin_dir("paths");
    write(&root.join("cmd.md"), "");
    write(&root.join("sub/x.md"), "");
    let outside = plugin_dir("paths-outside");
    write(&outside.join("x.md"), "");
    std::os::unix::fs::symlink(outside.join("x.md"), root.join("link.md")).unwrap();
    for bad in [
        "cmd.md",
        "./missing.md",
        "./sub/../cmd.md",
        "./link.md",
        "/etc/passwd",
        ".",
    ] {
        manifest(&root, &format!(r#"{{"name":"p","commands":"{bad}"}}"#));
        let err = load(&root).unwrap_err();
        assert!(err.contains(&format!("`{bad}`")), "{bad}: {err}");
    }
}

#[test]
fn a_manifest_without_a_name_is_rejected() {
    let root = plugin_dir("noname");
    manifest(&root, r#"{"version":"1"}"#);
    assert!(load(&root).unwrap_err().contains("name"));
}

#[test]
fn unsupported_components_and_unknown_keys_warn_by_name() {
    let root = plugin_dir("warn");
    write(&root.join("output-styles/a.md"), "");
    manifest(
        &root,
        r#"{"name":"p","version":"1","lspServers":{},"monitors":[],"frobnicate":1}"#,
    );
    let warnings = load(&root).unwrap().warnings.join("\n");
    for name in ["lspServers", "monitors", "frobnicate", "output-styles"] {
        assert!(warnings.contains(name), "{name} missing from {warnings}");
    }
    assert!(!warnings.contains("version"));
}
