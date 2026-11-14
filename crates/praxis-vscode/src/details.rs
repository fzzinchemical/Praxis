//! The plugin details tab (SPEC §12, T10.9): builds the page from a listed plugin. Pure, so it
//! tests natively. Plugin text is untrusted: names and state are escaped, and every markdown
//! text (the README and component descriptions) is rendered by the sanitizer with raw HTML
//! dropped and unsafe links removed, so one format shows and no markup leaks as text.

use crate::plugins::PluginItem;
// The chat panel's sanitizer, shared as a file because praxis-vscode-ui is a cdylib.
#[path = "../../praxis-vscode-ui/src/sanitize.rs"]
mod sanitize;
use sanitize::markdown_dropping_html as markdown;

/// The whole page for one plugin: its README, then its commands, skills and agents.
pub fn details_page(item: &PluginItem) -> String {
    let readme = match &item.readme {
        Some(text) => markdown(text),
        None => "<p>No README.</p>".to_owned(),
    };
    let components: String = item
        .components
        .iter()
        .map(|c| {
            format!(
                "<li><strong>{}</strong> <em>{}</em>: {}</li>\n",
                escape(&c.name),
                c.kind.label(),
                markdown(&c.description).trim_end()
            )
        })
        .collect();
    let components = if components.is_empty() {
        "<p>No commands, skills or agents.</p>".to_owned()
    } else {
        format!("<ul>\n{components}</ul>")
    };
    let settings = settings_list(item);
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'\">\n\
<title>{name}</title>\n\
<style>body {{ font-family: var(--vscode-font-family); color: var(--vscode-foreground); \
background: var(--vscode-editor-background); line-height: 1.5; max-width: 60rem; margin: 0 auto; padding: 1rem; }}\n\
code, pre {{ font-family: var(--vscode-editor-font-family); }}\n\
.badge {{ border: 1px solid var(--vscode-panel-border); border-radius: 3px; padding: 0 0.4em; font-weight: bold; }}</style>\n\
</head>\n<body>\n<main>\n<h1>{name}</h1>\n<p>Status: <span class=\"badge\">{state}</span></p>\n\
<section aria-labelledby=\"readme\"><h2 id=\"readme\">README</h2>\n{readme}</section>\n\
<section aria-labelledby=\"components\"><h2 id=\"components\">Commands, skills and agents</h2>\n{components}</section>\n\
<section aria-labelledby=\"settings\"><h2 id=\"settings\">Settings</h2>\n{settings}</section>\n\
</main>\n</body>\n</html>\n",
        name = escape(&item.name),
        state = escape(&item.description()),
    )
}

/// Each `userConfig` key with its description, current value and a link that asks for a new one.
/// The link is a `command:` URI, which the webview allows without scripts; its single argument is
/// a JSON object, so the handler reads named fields. A sensitive value is never shown.
fn settings_list(item: &PluginItem) -> String {
    if item.settings.is_empty() {
        return "<p>No settings.</p>".to_owned();
    }
    let rows: String = item
        .settings
        .iter()
        .map(|s| {
            let value = match (s.sensitive, &s.value) {
                (true, _) if s.set => "set (hidden)".to_owned(),
                (true, _) => "not set".to_owned(),
                (false, Some(value)) => value.clone(),
                (false, None) => "not set".to_owned(),
            };
            let arg = serde_json::json!({"plugin": item.name, "key": s.key}).to_string();
            format!(
                "<li><strong>{key}</strong>: {description}<br>Current value: {value} \
<a href=\"command:praxis.setPluginOption?{arg}\" aria-label=\"Change {key}\">Change</a></li>\n",
                key = escape(&s.key),
                description = escape(&s.description),
                value = escape(&value),
                arg = percent_encode(&arg),
            )
        })
        .collect();
    format!("<ul>\n{rows}</ul>")
}

/// Percent-encodes everything but unreserved characters, as a command URI's query needs.
fn percent_encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(b).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::{Component, Kind, Setting};

    fn plugin(readme: Option<&str>, components: Vec<Component>) -> PluginItem {
        PluginItem {
            name: "review".to_owned(),
            enabled: true,
            commit: "abc1234".to_owned(),
            can_run: Vec::new(),
            error: None,
            readme: readme.map(str::to_owned),
            components,
            settings: Vec::new(),
        }
    }

    fn setting(key: &str, value: Option<&str>, sensitive: bool) -> Setting {
        Setting {
            key: key.to_owned(),
            description: format!("About {key}"),
            sensitive,
            set: value.is_some(),
            value: value.map(str::to_owned),
        }
    }

    fn command(name: &str, description: &str) -> Component {
        Component {
            kind: Kind::Command,
            name: name.to_owned(),
            description: description.to_owned(),
        }
    }

    #[test]
    fn renders_the_readme_as_markdown() {
        let page = details_page(&plugin(Some("# Review\n\n**Checks** diffs."), Vec::new()));
        assert!(page.contains("<h1>Review</h1>"), "{page}");
        assert!(page.contains("<strong>Checks</strong>"), "{page}");
    }

    #[test]
    fn drops_raw_html_from_the_readme_without_showing_it() {
        let page = details_page(&plugin(
            Some(
                "<script>alert(1)</script>\n\n<img src=x onerror=alert(2)>\n\n<p align=\"center\">x</p>",
            ),
            Vec::new(),
        ));
        assert!(!page.contains("<script>alert"), "{page}");
        assert!(!page.contains("<img src"), "{page}");
        assert!(!page.contains("&lt;"), "{page}");
    }

    #[test]
    fn renders_no_raw_markdown_markers_in_readme_or_descriptions() {
        let page = details_page(&plugin(
            Some("# Review\n\n**Checks** `diffs`."),
            vec![command("review:fmt", "Formats **code** with `fmt`")],
        ));
        assert!(page.contains("<h1>Review</h1>"), "{page}");
        assert!(page.contains("<strong>code</strong>"), "{page}");
        assert!(page.contains("<code>fmt</code>"), "{page}");
        assert!(!page.contains("**"), "{page}");
        assert!(!page.contains('`'), "{page}");
        assert!(!page.contains("# Review"), "{page}");
    }

    #[test]
    fn shows_the_enabled_state_as_a_badge() {
        let page = details_page(&plugin(None, Vec::new()));
        assert!(
            page.contains(r#"Status: <span class="badge">enabled</span>"#),
            "{page}"
        );
    }

    #[test]
    fn drops_unsafe_links_in_the_readme() {
        let page = details_page(&plugin(
            Some("[bad](javascript:alert(1)) [ok](https://example.com)"),
            Vec::new(),
        ));
        assert!(!page.contains("javascript:"), "{page}");
        assert!(page.contains(r#"href="https://example.com""#), "{page}");
    }

    #[test]
    fn escapes_component_names_and_descriptions() {
        let page = details_page(&plugin(
            None,
            vec![command("review:<b>x</b>", "says <img src=x>")],
        ));
        assert!(!page.contains("<b>x</b>"), "{page}");
        assert!(!page.contains("<img src=x>"), "{page}");
        assert!(page.contains("review:&lt;b&gt;x&lt;/b&gt;"), "{page}");
    }

    #[test]
    fn a_plugin_without_a_readme_still_lists_its_commands() {
        let page = details_page(&plugin(None, vec![command("review:hello", "Says hi")]));
        assert!(page.contains("No README"), "{page}");
        assert!(page.contains("review:hello"), "{page}");
        assert!(page.contains("Says hi"), "{page}");
    }

    #[test]
    fn lists_each_setting_with_its_description_and_current_value() {
        let mut item = plugin(None, Vec::new());
        item.settings = vec![
            setting("level", Some("high"), false),
            setting("mode", None, false),
        ];
        let page = details_page(&item);
        assert!(page.contains("<h2 id=\"settings\">Settings</h2>"), "{page}");
        assert!(
            page.contains("<strong>level</strong>: About level"),
            "{page}"
        );
        assert!(page.contains("Current value: high"), "{page}");
        assert!(page.contains("Current value: not set"), "{page}");
    }

    #[test]
    fn a_sensitive_setting_shows_only_whether_it_is_set() {
        let mut item = plugin(None, Vec::new());
        item.settings = vec![setting("token", Some("s3cret"), true)];
        let page = details_page(&item);
        assert!(!page.contains("s3cret"), "{page}");
        assert!(page.contains("Current value: set (hidden)"), "{page}");
    }

    #[test]
    fn change_link_passes_the_plugin_and_key_as_one_object() {
        let mut item = plugin(None, Vec::new());
        item.settings = vec![setting("level", None, false)];
        let page = details_page(&item);
        // {"key":"level","plugin":"review"} (keys sorted), percent-encoded.
        assert!(
            page.contains(
                "command:praxis.setPluginOption?%7B%22key%22%3A%22level%22%2C%22plugin%22%3A%22review%22%7D"
            ),
            "{page}"
        );
    }

    #[test]
    fn says_so_when_a_plugin_has_no_settings() {
        let page = details_page(&plugin(None, Vec::new()));
        assert!(page.contains("No settings."), "{page}");
    }

    #[test]
    fn the_page_allows_no_scripts_and_no_remote_resources() {
        let page = details_page(&plugin(Some("text"), Vec::new()));
        assert!(
            page.contains("default-src 'none'; style-src 'unsafe-inline'"),
            "{page}"
        );
    }
}
