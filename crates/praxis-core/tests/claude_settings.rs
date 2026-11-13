//! Claude Code's settings files through the public interface (SPEC §8, §9, §12). User files
//! apply without approval; project files widen only after approval, and their deny rules apply
//! at once.

// Lets clippy treat helper functions here as test code for `allow-unwrap-in-tests`.
#![cfg(test)]

use praxis_core::config::{Loaded, Sources, load_with_approval};
use praxis_core::trust::TrustStore;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

fn temp(kind: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("praxis-claude-{kind}-{}-{id}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path.canonicalize().unwrap()
}

fn put(path: &Path, text: &str) -> PathBuf {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    path.to_path_buf()
}

fn hooks(command: &str) -> Value {
    json!({"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": command}]}]})
}

/// Loads with one project config and project settings files; returns the prompts shown.
fn load_project(
    project: &Path,
    claude: Vec<PathBuf>,
    store: &TrustStore,
    answer: bool,
) -> (Loaded, Vec<String>) {
    let mut prompts = Vec::new();
    let loaded = load_with_approval(
        Sources {
            project: Some(project.to_path_buf()),
            claude_project: claude,
            trust: Some(store.clone()),
            ..Sources::default()
        },
        &mut |_: &Path, text: &str| {
            prompts.push(text.to_owned());
            answer
        },
    )
    .unwrap();
    (loaded, prompts)
}

fn load_project_err(project: &Path, claude: Vec<PathBuf>, store: &TrustStore) -> String {
    load_with_approval(
        Sources {
            project: Some(project.to_path_buf()),
            claude_project: claude,
            trust: Some(store.clone()),
            ..Sources::default()
        },
        &mut |_: &Path, _: &str| false,
    )
    .unwrap_err()
    .to_string()
}

#[test]
fn user_claude_settings_apply_without_approval() {
    let base = temp("user");
    let settings = put(
        &base.join("home/.claude/settings.json"),
        &json!({
            "permissions": {"allow": ["Bash(cargo test*)"], "deny": ["Read(**/.env*)"]},
            "hooks": hooks("true"),
            "model": "ignored", "theme": "dark"
        })
        .to_string(),
    );
    let user = put(
        &base.join("xdg/praxis/config.toml"),
        "[permissions]\ndeny = [\"Write\"]\n",
    );
    let loaded = load_with_approval(
        Sources {
            user: Some(user),
            claude_user: Some(settings),
            ..Sources::default()
        },
        &mut |_: &Path, _: &str| panic!("user settings need no approval"),
    )
    .unwrap();
    // Settings sit below the praxis user layer, so their rules come first in the merged list.
    assert_eq!(loaded.config.permissions.allow, ["Bash(cargo test*)"]);
    assert_eq!(loaded.config.permissions.deny, ["Read(**/.env*)", "Write"]);
    assert_eq!(loaded.config.hooks.len(), 1);
    assert_eq!(loaded.config.hooks[0].event, "PreToolUse");
    assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
}

#[test]
fn malformed_user_claude_settings_is_an_error_naming_the_file() {
    let base = temp("malformed");
    let bad = put(&base.join("settings.json"), "{ \"permissions\": ");
    let err = load_with_approval(
        Sources {
            claude_user: Some(bad.clone()),
            ..Sources::default()
        },
        &mut |_: &Path, _: &str| false,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains(&bad.display().to_string()), "{err}");
}

#[test]
fn a_malformed_project_settings_file_is_an_error_naming_the_file() {
    let base = temp("badproject");
    let bad = put(&base.join(".claude/settings.local.json"), "[");
    let store = TrustStore::new(base.join("trust.json"));
    let err = load_project_err(&base.join("config.toml"), vec![bad.clone()], &store);
    assert!(err.contains(&bad.display().to_string()), "{err}");
}

#[test]
fn project_deny_applies_but_allow_and_hooks_wait_for_approval() {
    let base = temp("project");
    let project = base.join("config.toml");
    let settings = put(
        &base.join(".claude/settings.json"),
        &json!({
            "permissions": {"allow": ["Bash"], "deny": ["Read(**/.env*)"]},
            "hooks": hooks("true")
        })
        .to_string(),
    );
    let store = TrustStore::new(base.join("trust.json"));

    let (loaded, prompts) = load_project(&project, vec![settings.clone()], &store, false);
    assert_eq!(prompts.len(), 1);
    assert!(
        prompts[0].contains("allow = ") && prompts[0].contains("PreToolUse"),
        "{}",
        prompts[0]
    );
    assert_eq!(loaded.config.permissions.deny, ["Read(**/.env*)"]);
    assert!(loaded.config.permissions.allow.is_empty());
    assert!(loaded.config.hooks.is_empty());
    assert!(!loaded.notices.is_empty());

    let (loaded, prompts) = load_project(&project, vec![settings.clone()], &store, true);
    assert_eq!(prompts.len(), 1);
    assert_eq!(loaded.config.permissions.allow, ["Bash"]);
    assert_eq!(loaded.config.hooks.len(), 1);

    // The approval is stored, so the same items load without asking again.
    let (loaded, prompts) = load_project(&project, vec![settings.clone()], &store, false);
    assert!(prompts.is_empty());
    assert_eq!(loaded.config.permissions.allow, ["Bash"]);
    assert_eq!(loaded.config.hooks.len(), 1);

    // Changed hooks need approval again; until then only the deny rule applies.
    let changed = fs::read_to_string(&settings)
        .unwrap()
        .replace("\"true\"", "\"false\"");
    fs::write(&settings, changed).unwrap();
    let (loaded, prompts) = load_project(&project, vec![settings], &store, false);
    assert_eq!(prompts.len(), 1);
    assert!(loaded.config.permissions.allow.is_empty());
    assert!(loaded.config.hooks.is_empty());
    assert_eq!(loaded.config.permissions.deny, ["Read(**/.env*)"]);
}

#[test]
fn a_hostile_repositorys_claude_settings_widen_nothing_unapproved() {
    let base = temp("hostile");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/hostile-repo/.claude/settings.json");
    let settings = put(
        &base.join(".claude/settings.json"),
        &fs::read_to_string(fixture).unwrap(),
    );
    let store = TrustStore::new(base.join("trust.json"));

    let (loaded, prompts) = load_project(&base.join("config.toml"), vec![settings], &store, false);
    assert_eq!(prompts.len(), 1);
    assert!(loaded.config.permissions.allow.is_empty());
    assert!(loaded.config.hooks.is_empty());
}
