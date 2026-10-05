//! Turning note text into safe HTML and line diffs.

use crate::note::body;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Same,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

impl DiffLine {
    pub fn class(&self) -> &'static str {
        match self.kind {
            DiffKind::Same => "same",
            DiffKind::Added => "add",
            DiffKind::Removed => "rm",
        }
    }

    pub fn sign(&self) -> &'static str {
        match self.kind {
            DiffKind::Same => " ",
            DiffKind::Added => "+",
            DiffKind::Removed => "-",
        }
    }
}

/// Markdown to HTML. Raw HTML in the input is shown as text, never rendered.
pub fn markdown(md: &str) -> String {
    use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, html};

    fn safe_url(url: CowStr<'_>) -> CowStr<'_> {
        let lower = url.trim().to_ascii_lowercase();
        let scheme_ok = ["http://", "https://", "mailto:"]
            .iter()
            .any(|p| lower.starts_with(p));
        if scheme_ok || !lower.contains(':') {
            url
        } else {
            CowStr::Borrowed("#")
        }
    }

    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let events = Parser::new_ext(md, opts).map(|ev| match ev {
        Event::Html(t) | Event::InlineHtml(t) => Event::Text(t),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url: safe_url(dest_url),
            title,
            id,
        }),
        Event::Start(Tag::Image {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Image {
            link_type,
            dest_url: safe_url(dest_url),
            title,
            id,
        }),
        other => other,
    });
    let mut out = String::new();
    html::push_html(&mut out, events);
    out
}

/// Line diff between the body of a raw note (frontmatter dropped) and a draft.
pub fn diff(old_raw: &str, new: &str) -> Vec<DiffLine> {
    use similar::{ChangeTag, TextDiff};

    let old = body(old_raw);
    TextDiff::from_lines(old, new)
        .iter_all_changes()
        .map(|c| DiffLine {
            kind: match c.tag() {
                ChangeTag::Equal => DiffKind::Same,
                ChangeTag::Insert => DiffKind::Added,
                ChangeTag::Delete => DiffKind::Removed,
            },
            text: c.value().trim_end_matches(['\n', '\r']).to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_renders_lists_and_code() {
        let h = markdown("- [fact] one\n- two\n\n`code`");
        assert!(h.contains("<li>[fact] one</li>"), "{h}");
        assert!(h.contains("<code>code</code>"), "{h}");
    }

    #[test]
    fn markdown_strips_raw_html() {
        let h =
            markdown("hi <script>alert(1)</script>\n\n<b>x</b>\n\n<img src=x onerror=alert(1)>");
        assert!(!h.contains("<script>"), "{h}");
        assert!(!h.contains("<b>"), "{h}");
        assert!(!h.contains("<img"), "{h}");
        assert!(h.contains("&lt;script&gt;"), "{h}");
    }

    #[test]
    fn markdown_neutralises_javascript_links() {
        let h = markdown("[click](javascript:alert(1)) ![i](javascript:alert(2))");
        assert!(!h.contains("javascript:"), "{h}");
    }

    #[test]
    fn diff_marks_added_and_removed() {
        let old = "---\ntitle: T\n---\n\nkeep\nold line\n";
        let d = diff(old, "keep\nnew line\n");
        let kinds: Vec<(DiffKind, &str)> = d.iter().map(|l| (l.kind, l.text.as_str())).collect();
        assert_eq!(
            kinds,
            [
                (DiffKind::Same, "keep"),
                (DiffKind::Removed, "old line"),
                (DiffKind::Added, "new line")
            ]
        );
    }

    #[test]
    fn diff_ignores_frontmatter() {
        let d = diff("---\ntitle: T\n---\nsame", "same");
        assert!(d.iter().all(|l| l.kind == DiffKind::Same));
    }
}
