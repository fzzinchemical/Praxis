//! Untrusted markdown to safe HTML. Also compiled into `praxis-vscode` through `#[path]`.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd, html};

/// Renders untrusted model markdown to HTML with raw HTML escaped and only safe link schemes kept.
#[allow(dead_code)] // praxis-vscode's details page uses only `markdown_dropping_html`, through #[path].
pub fn markdown(text: &str) -> String {
    render(text, false)
}

/// Like `markdown`, but raw HTML is dropped instead of shown as escaped text. README pages use it,
/// since their HTML is layout, not text to read. shortcut: text inside an HTML block is dropped
/// too, upgrade with a tag allowlist if a README's `<details>` text matters.
#[allow(dead_code)] // The chat panel never calls it; praxis-vscode's details page does, through #[path].
pub fn markdown_dropping_html(text: &str) -> String {
    render(text, true)
}

fn render(text: &str, drop_html: bool) -> String {
    // CommonMark links can't nest, so one flag per kind tracks a dropped start tag.
    let (mut in_link, mut in_image) = (false, false);
    let events = Parser::new_ext(text, Options::ENABLE_TABLES).filter_map(|event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) => (!drop_html).then_some(Event::Text(raw)),
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
