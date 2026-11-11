#![cfg(test)]

use harness_core::config::McpServer;
use harness_core::plugin::{Source, load, run_command};
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
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

#[test]
fn agents_read_tool_restrictions_and_turn_limits() {
    let root = plugin_dir("agents");
    manifest(&root, r#"{"name":"kit"}"#);
    write(
        &root.join("agents/team/scout.md"),
        "---\nname: scout\ndescription: Finds things\ntools: Read, Grep\ndisallowedTools: [Bash]\nmaxTurns: 3\n---\nYou scout.\n",
    );
    write(&root.join("agents/free.md"), "Do anything.\n");
    let plugin = load(&root).unwrap();

    let agents = plugin.agents();
    let scout = agents.iter().find(|a| a.skill.name == "kit:scout").unwrap();
    assert_eq!(scout.skill.description, "Finds things");
    assert_eq!(scout.skill.body, "You scout.\n");
    assert_eq!(
        scout.tools.as_deref(),
        Some(&["Read".to_owned(), "Grep".to_owned()][..])
    );
    assert_eq!(scout.disallowed_tools, ["Bash"]);
    assert_eq!(scout.max_turns, Some(3));
    let free = agents.iter().find(|a| a.skill.name == "kit:free").unwrap();
    assert_eq!((free.tools.clone(), free.max_turns), (None, None));
}

#[test]
fn skills_and_commands_read_frontmatter_and_commands_substitute_arguments() {
    let root = plugin_dir("components");
    manifest(&root, r#"{"name":"kit"}"#);
    write(
        &root.join("skills/review/SKILL.md"),
        "---\nname: review\ndescription: \"Review a diff\"\n---\nRead ${CLAUDE_PLUGIN_ROOT}/guide.md first.\n",
    );
    write(
        &root.join("commands/fix.md"),
        "---\ndescription: Fix it\n---\nFix issue $ARGUMENTS now.\n",
    );
    write(&root.join("commands/plain.md"), "Just do it.\n");
    let plugin = load(&root).unwrap();

    let skills = plugin.skills();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "kit:review");
    assert_eq!(skills[0].description, "Review a diff");
    assert_eq!(
        skills[0].body,
        format!("Read {}/guide.md first.\n", root.display())
    );

    let commands = plugin.commands();
    assert_eq!(commands[0].description, "Fix it");
    assert_eq!(
        run_command(&commands, "/kit:fix 42 and 43").as_deref(),
        Some("Fix issue 42 and 43 now.\n")
    );
    assert_eq!(
        run_command(&commands, "/kit:plain fast").as_deref(),
        Some("Just do it.\n\nARGUMENTS: fast")
    );
    assert_eq!(
        run_command(&commands, "/kit:plain").as_deref(),
        Some("Just do it.\n")
    );
    assert_eq!(run_command(&commands, "/kit:missing"), None);
    assert_eq!(run_command(&commands, "kit:fix"), None);
}

#[test]
fn block_scalar_descriptions_are_folded_into_one_line() {
    // As in DietrichGebert/ponytail's SKILL.md files (docs/compat.md).
    let root = plugin_dir("folded");
    manifest(&root, r#"{"name":"kit"}"#);
    write(
        &root.join("skills/help/SKILL.md"),
        "---\nname: help\ndescription: >\n  Quick reference.\n  Use for help.\nother: x\n---\nBody\n",
    );
    write(
        &root.join("skills/keep/SKILL.md"),
        "---\ndescription: |-\n  Line one\n\n  line two\n---\nBody\n",
    );
    write(
        &root.join("agents/scout.md"),
        "---\ndescription: >-\n  Finds\n  things\ntools: Read\n---\nBody\n",
    );
    let plugin = load(&root).unwrap();
    let skills = plugin.skills();
    assert_eq!(skills[0].description, "Quick reference. Use for help.");
    assert_eq!(skills[1].description, "Line one line two");
    let agents = plugin.agents();
    assert_eq!(agents[0].skill.description, "Finds things");
    assert_eq!(agents[0].tools.as_deref(), Some(&["Read".to_owned()][..]));
}

#[test]
fn hook_files_warn_about_keys_other_than_hooks() {
    // ponytail's `claude-band.json` adds `modules` (JS mods, unsupported by ADR 0001).
    let root = plugin_dir("hookkeys");
    manifest(&root, r#"{"name":"kit","hooks":["./band.json"]}"#);
    write(
        &root.join("band.json"),
        r#"{"hooks":{},"modules":["./x.tsx"]}"#,
    );
    let (hooks, warnings) = load(&root).unwrap().hooks().unwrap();
    assert!(hooks.is_empty());
    assert!(
        warnings.iter().any(|w| w.contains("`modules`")),
        "{warnings:?}"
    );
}

#[test]
fn plugin_mcp_servers_expand_variables_and_take_the_plugin_name() {
    let root = plugin_dir("mcp");
    let data = plugin_dir("mcp-data");
    manifest(
        &root,
        r#"{"name": "kit", "userConfig": {"token": {"sensitive": true}, "level": {}},
            "mcpServers": {"inline": {"command": "srv2"}}}"#,
    );
    write(
        &root.join(".mcp.json"),
        r#"{"mcpServers": {"db": {"command": "${CLAUDE_PLUGIN_ROOT}/srv",
            "args": ["${CLAUDE_PLUGIN_DATA}", "${CLAUDE_PROJECT_DIR}", "${CLAUDE_PLUGIN_OPTION_LEVEL}"],
            "env": {"TOKEN": "${CLAUDE_PLUGIN_OPTION_TOKEN}"}}}}"#,
    );
    fs::create_dir_all(root.join("bin")).unwrap();
    let plugin = load(&root).unwrap();
    plugin.set_option(&data, "token", "s3cret").unwrap();
    plugin.set_option(&data, "level", "high").unwrap();

    let servers = plugin.mcp_servers(Path::new("/proj"), &data).unwrap();
    let names: Vec<_> = servers.keys().cloned().collect();
    assert_eq!(names, ["plugin_kit_db", "plugin_kit_inline"]);
    let McpServer::Stdio(db) = &servers["plugin_kit_db"] else {
        panic!("{servers:?}")
    };
    let data_dir = data.join("kit");
    assert_eq!(db.command, format!("{}/srv", root.display()));
    assert_eq!(db.args, [data_dir.to_str().unwrap(), "/proj", "high"]);
    assert_eq!(db.env["TOKEN"], "s3cret");
    assert_eq!(db.env["CLAUDE_PLUGIN_ROOT"], root.to_str().unwrap());
    assert_eq!(db.env["CLAUDE_PLUGIN_OPTION_TOKEN"], "s3cret");

    // Sensitive values stay out of logs and live only in a 0600 file.
    assert!(!format!("{servers:?}").contains("s3cret"), "{servers:?}");
    let options = data_dir.join("options.json");
    let mode = fs::metadata(&options).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    // `bin/` comes after the user's PATH, so it can't shadow system commands.
    let path = db.env["PATH"].clone();
    let bin = root.join("bin");
    assert!(path.ends_with(&format!(":{}", bin.display())), "{path}");
    assert!(path.starts_with(&std::env::var("PATH").unwrap()), "{path}");
}

#[test]
fn unknown_user_config_keys_are_rejected() {
    let root = plugin_dir("mcp-unknown");
    manifest(&root, r#"{"name": "kit", "userConfig": {"level": {}}}"#);
    let plugin = load(&root).unwrap();
    assert!(
        plugin
            .set_option(&plugin_dir("unk-data"), "other", "x")
            .is_err()
    );
}
