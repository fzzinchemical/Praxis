//! The plugin details tab (SPEC §12, T10.9): builds the page from a listed plugin. Pure, so it
//! tests natively. Plugin text is untrusted: everything but the README is escaped, and the README
//! is rendered with raw HTML escaped and unsafe links dropped.

use crate::plugins::PluginItem;
use pulldown_cmark::{Event, Parser, Tag, TagEnd, html};

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
                escape(&c.description)
            )
        })
        .collect();
    let components = if components.is_empty() {
        "<p>No commands, skills or agents.</p>".to_owned()
    } else {
        format!("<ul>\n{components}</ul>")
    };
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'\">\n\
<title>{name}</title>\n\
<style>body {{ font-family: var(--vscode-font-family); color: var(--vscode-foreground); \
background: var(--vscode-editor-background); line-height: 1.5; max-width: 60rem; margin: 0 auto; padding: 1rem; }}\n\
code, pre {{ font-family: var(--vscode-editor-font-family); }}</style>\n\
</head>\n<body>\n<main>\n<h1>{name}</h1>\n<p>{state}</p>\n\
<section aria-labelledby=\"readme\"><h2 id=\"readme\">README</h2>\n{readme}</section>\n\
<section aria-labelledby=\"components\"><h2 id=\"components\">Commands, skills and agents</h2>\n{components}</section>\n\
</main>\n</body>\n</html>\n",
        name = escape(&item.name),
        state = escape(&item.description()),
    )
}

/// Renders untrusted markdown with raw HTML shown as text and only safe link schemes kept.
/// Same rules as the chat panel's `markdown` in praxis-vscode-ui, which cannot be imported here
/// (it is a cdylib). shortcut: duplicated rules; move both into one rlib crate if they change.
fn markdown(text: &str) -> String {
    // CommonMark links can't nest, so one flag per kind tracks a dropped start tag.
    let (mut in_link, mut in_image) = (false, false);
    let events = Parser::new(text).filter_map(|event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) => Some(Event::Text(raw)),
        Event::Start(Tag::Link { ref dest_url, .. }) if !safe_url(dest_url) => {
            in_link = true;
            None
        }
        Event::End(TagEnd::Link) if in_link => {
            in_link = false;
            None
        }
        // Images never load: the CSP blocks them, and alt text is enough.
        Event::Start(Tag::Image { .. }) => {
            in_image = true;
            None
        }
        Event::End(TagEnd::Image) if in_image => {
            in_image = false;
            None
        }
        other => Some(other),
    });
    let mut out = String::new();
    html::push_html(&mut out, events);
    out
}

fn safe_url(url: &str) -> bool {
    let url = url.to_ascii_lowercase();
    ["http://", "https://", "vscode:"]
        .iter()
        .any(|scheme| url.starts_with(scheme))
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
    use crate::plugins::{Component, Kind};

    fn plugin(readme: Option<&str>, components: Vec<Component>) -> PluginItem {
        PluginItem {
            name: "review".to_owned(),
            enabled: true,
            commit: "abc1234".to_owned(),
            can_run: Vec::new(),
            error: None,
            readme: readme.map(str::to_owned),
            components,
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
    fn keeps_raw_html_in_the_readme_escaped() {
        let page = details_page(&plugin(
            Some("<script>alert(1)</script>\n\n<img src=x onerror=alert(2)>"),
            Vec::new(),
        ));
        assert!(!page.contains("<script>alert"), "{page}");
        assert!(!page.contains("<img src"), "{page}");
        assert!(page.contains("&lt;script&gt;"), "{page}");
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
    fn the_page_allows_no_scripts_and_no_remote_resources() {
        let page = details_page(&plugin(Some("text"), Vec::new()));
        assert!(
            page.contains("default-src 'none'; style-src 'unsafe-inline'"),
            "{page}"
        );
    }
}
