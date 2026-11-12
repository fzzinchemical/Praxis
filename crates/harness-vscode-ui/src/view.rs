//! Pure rendering for the panel, kept free of the DOM so it tests natively.

use pulldown_cmark::{Event, Parser, Tag, TagEnd, html};

/// Renders untrusted model markdown to HTML with raw HTML escaped and only safe link schemes kept.
pub fn markdown(text: &str) -> String {
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

/// What the agent is doing, for the status line.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Phase {
    Idle,
    ProcessingPrompt,
    Generating,
    RunningTool,
}

#[derive(Debug, PartialEq)]
pub struct Status {
    pub phase: Phase,
    /// Context tokens used and the window size, from the last `usage` message.
    pub context: Option<(u64, u64)>,
}

impl Status {
    pub fn line(&self, elapsed_secs: u64) -> String {
        let phase = match self.phase {
            Phase::Idle => "Idle",
            Phase::ProcessingPrompt => "Processing prompt",
            Phase::Generating => "Generating",
            Phase::RunningTool => "Running tool",
        };
        let time = format!("{}:{:02}", elapsed_secs / 60, elapsed_secs % 60);
        let context = match self.context {
            Some((used, size)) if size > 0 => {
                format!("context {used} / {size} ({}%)", used * 100 / size)
            }
            Some((used, size)) => format!("context {used} / {size}"),
            None => "context unknown".to_owned(),
        };
        format!("{phase} · {time} · {context}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_tags_in_model_text_render_inert() {
        for text in [
            "<script>alert(1)</script>",
            "hi <script>alert(1)</script>",
            "<img src=x onerror=alert(1)>",
        ] {
            let out = markdown(text);
            assert!(!out.contains("<script"), "{out}");
            assert!(!out.contains("<img"), "{out}");
        }
    }

    #[test]
    fn javascript_links_render_inert() {
        for text in [
            "[x](javascript:alert(1))",
            "[x](JavaScript:alert(1))",
            "[x]: javascript:alert(1)\n\n[x]",
            "<javascript:alert(1)>",
            "![x](javascript:alert(1))",
            "[x](data:text/html,hi)",
        ] {
            let out = markdown(text);
            assert!(!out.contains("href"), "{text} -> {out}");
            assert!(!out.contains("src"), "{text} -> {out}");
            assert!(out.contains('x') || out.contains("javascript"), "{out}");
        }
    }

    #[test]
    fn safe_links_and_formatting_survive() {
        let out = markdown("**b** [docs](https://example.com) [v](vscode:extension/x) `c`");
        assert!(out.contains("<strong>b</strong>"), "{out}");
        assert!(out.contains(r#"href="https://example.com""#), "{out}");
        assert!(out.contains(r#"href="vscode:extension/x""#), "{out}");
        assert!(out.contains("<code>c</code>"), "{out}");
    }

    #[test]
    fn status_line_shows_phase_elapsed_time_and_context_used() {
        let status = Status {
            phase: Phase::Generating,
            context: Some((5_000, 32_768)),
        };
        assert_eq!(
            status.line(75),
            "Generating · 1:15 · context 5000 / 32768 (15%)"
        );
        let status = Status {
            phase: Phase::ProcessingPrompt,
            context: None,
        };
        assert_eq!(status.line(3), "Processing prompt · 0:03 · context unknown");
        let status = Status {
            phase: Phase::RunningTool,
            context: Some((0, 0)),
        };
        assert_eq!(status.line(0), "Running tool · 0:00 · context 0 / 0");
    }
}
