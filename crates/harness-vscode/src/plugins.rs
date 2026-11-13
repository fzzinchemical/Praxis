//! The Plugins view's pure logic (SPEC §12). The extension shells out to the CLI, so this module
//! only maps `plugin list --json` rows and builds argv; it never touches plugin files.

use serde_json::Value;

/// One installed plugin, as `harness plugin list --json` prints it.
#[derive(Debug, PartialEq)]
pub struct PluginItem {
    pub name: String,
    pub enabled: bool,
    pub commit: String,
    pub can_run: Vec<String>,
}

impl PluginItem {
    /// The state, plus the pinned commit while disabled.
    pub fn description(&self) -> String {
        if self.enabled {
            return "enabled".to_owned();
        }
        let short: String = self.commit.chars().take(7).collect();
        format!("disabled @ {short}")
    }

    /// The modal text shown before enabling: what the plugin can run.
    pub fn enable_prompt(&self) -> String {
        if self.can_run.is_empty() {
            return format!("Enable {}? It can run nothing.", self.name);
        }
        let lines: Vec<String> = self
            .can_run
            .iter()
            .map(|line| format!("  {line}"))
            .collect();
        format!("Enable {}? It can run:\n{}", self.name, lines.join("\n"))
    }
}

/// Maps a `plugin list --json` answer. One malformed row fails the whole listing rather than hiding a plugin.
pub fn plugin_items(json: &str) -> Result<Vec<PluginItem>, String> {
    let Value::Array(rows) = serde_json::from_str(json).map_err(|error| error.to_string())? else {
        return Err("plugin list did not print an array".to_owned());
    };
    rows.iter().map(plugin_item).collect()
}

fn plugin_item(row: &Value) -> Result<PluginItem, String> {
    let text = |key: &str| {
        row[key]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("plugin row without a string `{key}`"))
    };
    let can_run = row["can_run"]
        .as_array()
        .ok_or("plugin row without a `can_run` array")?
        .iter()
        .map(|line| {
            line.as_str()
                .map(str::to_owned)
                .ok_or_else(|| "`can_run` entry is not a string".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PluginItem {
        name: text("name")?,
        enabled: row["enabled"]
            .as_bool()
            .ok_or("plugin row without an `enabled` flag")?,
        commit: text("commit")?,
        can_run,
    })
}

/// The actions that take a plugin name.
#[derive(Clone, Copy)]
pub enum Verb {
    Enable,
    Disable,
    Remove,
}

/// `--` ends option parsing, so a name or source can't be read as a flag.
pub fn verb_args(verb: Verb, name: &str) -> Vec<String> {
    let word = match verb {
        Verb::Enable => "enable",
        Verb::Disable => "disable",
        Verb::Remove => "remove",
    };
    args(&[word, "--", name])
}

pub fn list_args() -> Vec<String> {
    args(&["list", "--json"])
}

pub fn install_args(source: &str) -> Vec<String> {
    args(&["install", "--", source])
}

fn args(rest: &[&str]) -> Vec<String> {
    std::iter::once("plugin")
        .chain(rest.iter().copied())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = r#"[
        {"name":"review","source":"/src/review","commit":"1a2b3c4d5e","enabled":true,
         "can_run":["hook: PreToolUse Bash","bin/lint"]},
        {"name":"notes","source":"https://example.com/notes.git","commit":"9f8e7d6c5b",
         "enabled":false,"can_run":[]}
    ]"#;

    #[test]
    fn maps_each_listed_plugin_to_an_item() {
        let items = plugin_items(LISTING).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "review");
        assert_eq!(items[0].can_run, ["hook: PreToolUse Bash", "bin/lint"]);
        assert!(!items[1].enabled);
    }

    #[test]
    fn description_shows_the_short_commit_only_while_disabled() {
        let items = plugin_items(LISTING).unwrap();
        assert_eq!(items[0].description(), "enabled");
        assert_eq!(items[1].description(), "disabled @ 9f8e7d6");
    }

    #[test]
    fn empty_listing_has_no_items() {
        assert!(plugin_items("[]").unwrap().is_empty());
    }

    #[test]
    fn rejects_a_row_without_a_name() {
        assert!(plugin_items(r#"[{"enabled":true,"commit":"x","can_run":[]}]"#).is_err());
    }

    #[test]
    fn rejects_output_that_is_not_an_array() {
        assert!(plugin_items(r#"{"name":"review"}"#).is_err());
    }

    #[test]
    fn enable_prompt_lists_what_the_plugin_can_run() {
        let items = plugin_items(LISTING).unwrap();
        assert_eq!(
            items[0].enable_prompt(),
            "Enable review? It can run:\n  hook: PreToolUse Bash\n  bin/lint"
        );
    }

    #[test]
    fn enable_prompt_says_when_nothing_can_run() {
        let items = plugin_items(LISTING).unwrap();
        assert_eq!(
            items[1].enable_prompt(),
            "Enable notes? It can run nothing."
        );
    }

    #[test]
    fn named_actions_end_option_parsing_before_the_name() {
        assert_eq!(
            verb_args(Verb::Enable, "--delete-data"),
            ["plugin", "enable", "--", "--delete-data"]
        );
    }

    #[test]
    fn install_passes_the_source_after_option_parsing_ends() {
        assert_eq!(
            install_args("https://example.com/p.git"),
            ["plugin", "install", "--", "https://example.com/p.git"]
        );
    }

    #[test]
    fn list_asks_for_json() {
        assert_eq!(list_args(), ["plugin", "list", "--json"]);
    }
}
