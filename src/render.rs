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

/// Byte range in the markdown source.
pub type Span = (usize, usize);

/// One highlight: where it is and the opening tag that wraps it.
pub struct Highlight {
    pub spans: Vec<Span>,
    pub open: String,
}

/// Removes Private Use Area characters, which the highlighter uses as markers.
pub fn strip_private_use(s: &str) -> String {
    s.chars()
        .filter(|c| !('\u{E000}'..='\u{F8FF}').contains(c))
        .collect()
}

/// Finds a quote in the markdown, line by line, first occurrence only.
/// Quotes shorter than 3 characters are not located.
pub fn locate(md: &str, quote: &str) -> Option<Vec<Span>> {
    if quote.trim().chars().count() < 3 {
        return None;
    }
    let mut spans = Vec::new();
    let mut from = 0;
    for line in quote.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (at, end) = match md[from..].find(line) {
            Some(i) => (from + i, from + i + line.len()),
            None => find_without_markup(md, from, line)?,
        };
        spans.push((at, end));
        from = end;
    }
    (!spans.is_empty()).then_some(spans)
}

/// Inline markup that disappears when markdown is rendered.
fn is_markup(c: char) -> bool {
    matches!(c, '`' | '*' | '_')
}

/// Finds `needle` in `md[from..]` ignoring inline markup on both sides, and
/// returns the byte range in `md`. Lets a quote copied from the rendered page
/// match its markdown source.
fn find_without_markup(md: &str, from: usize, needle: &str) -> Option<Span> {
    let mut plain = String::new();
    // For every byte of `plain`, the byte index in `md` it came from.
    let mut origin: Vec<usize> = Vec::new();
    for (i, c) in md[from..].char_indices() {
        if is_markup(c) {
            continue;
        }
        plain.push(c);
        origin.extend((0..c.len_utf8()).map(|k| from + i + k));
    }
    let needle: String = needle.chars().filter(|c| !is_markup(*c)).collect();
    if needle.is_empty() {
        return None;
    }
    let at = plain.find(&needle)?;
    let start = origin[at];
    let last = origin[at + needle.len() - 1];
    // Include a closing marker that directly follows the match (`x` → `x`).
    let mut end = last + 1;
    while md[end..].starts_with(is_markup) {
        end += 1;
    }
    Some((start, end))
}

/// Moves a span start past block syntax (`- `, `1. `, `# `, `> `) when the span
/// begins its line: a marker in front of that syntax would break the block.
fn skip_block_prefix(md: &str, start: usize, end: usize) -> usize {
    let line_start = md[..start].rfind('\n').map_or(0, |i| i + 1);
    if !md[line_start..start].trim().is_empty() {
        return start;
    }
    let mut pos = start;
    loop {
        let rest = &md[pos..end];
        let trimmed = rest.trim_start_matches([' ', '\t']);
        let ws = rest.len() - trimmed.len();
        let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
        let hashes = trimmed.chars().take_while(|c| *c == '#').count();
        let prefix = if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
            hashes + 1
        } else if trimmed.starts_with("> ") {
            2
        } else if trimmed.starts_with('>') {
            1
        } else if trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed.starts_with("+ ")
        {
            2
        } else if (1..=9).contains(&digits)
            && (trimmed[digits..].starts_with(". ") || trimmed[digits..].starts_with(") "))
        {
            digits + 2
        } else {
            return pos;
        };
        if pos + ws + prefix >= end {
            return pos;
        }
        pos += ws + prefix;
    }
}

/// Markdown to HTML with the given spans wrapped in `<mark>` tags.
pub fn markdown_highlighted(md: &str, highlights: &[Highlight]) -> String {
    use std::collections::BTreeMap;

    // Marker characters: U+E100+i opens highlight i, U+E200+i closes it.
    let highlights = &highlights[..highlights.len().min(255)];
    let marker = |base: u32, i: usize| char::from_u32(base + i as u32).unwrap_or('\u{E0FF}');
    // At one position, closing markers go before opening ones.
    let mut at: BTreeMap<usize, (String, String)> = BTreeMap::new();
    for (i, h) in highlights.iter().enumerate() {
        for &(start, end) in &h.spans {
            if start >= end
                || end > md.len()
                || !md.is_char_boundary(start)
                || !md.is_char_boundary(end)
            {
                continue;
            }
            let start = skip_block_prefix(md, start, end);
            at.entry(start).or_default().1.push(marker(0xE100, i));
            at.entry(end).or_default().0.push(marker(0xE200, i));
        }
    }
    let mut marked = String::with_capacity(md.len() + at.len() * 6);
    let mut last = 0;
    for (pos, (closes, opens)) in &at {
        marked.push_str(&md[last..*pos]);
        marked.push_str(closes);
        marked.push_str(opens);
        last = *pos;
    }
    marked.push_str(&md[last..]);

    let html = markdown(&marked);
    let mut out = String::with_capacity(html.len() + highlights.len() * 40);
    for c in html.chars() {
        let code = c as u32;
        match code {
            0xE100..=0xE1FE if (code - 0xE100) < highlights.len() as u32 => {
                out.push_str(&highlights[(code - 0xE100) as usize].open);
            }
            0xE200..=0xE2FE if (code - 0xE200) < highlights.len() as u32 => out.push_str("</mark>"),
            _ => out.push(c),
        }
    }
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

    fn hl(spans: Vec<Span>, n: usize) -> Highlight {
        Highlight {
            spans,
            open: format!(r#"<mark class="add" data-n="{n}">"#),
        }
    }

    #[test]
    fn locate_single_line() {
        assert_eq!(locate("- a b c\n- d", "a b c"), Some(vec![(2, 7)]));
    }

    #[test]
    fn locate_trims_quote() {
        assert_eq!(locate("- a b c", "  a b c \n"), Some(vec![(2, 7)]));
    }

    #[test]
    fn locate_multi_line() {
        assert_eq!(
            locate("xxx\nyyy\nzzz", "xxx\nzzz"),
            Some(vec![(0, 3), (8, 11)])
        );
        assert_eq!(locate("zzz\nxxx", "xxx\nzzz"), None, "order matters");
    }

    #[test]
    fn locate_missing_or_tiny() {
        assert!(locate("abc", "zzz").is_none());
        assert!(locate("abc", "ab").is_none());
        assert!(locate("abc", "   ").is_none());
    }

    #[test]
    fn locate_first_occurrence_only() {
        assert_eq!(locate("dup and dup", "dup"), Some(vec![(0, 3)]));
    }

    #[test]
    fn highlighted_wraps_text_in_mark() {
        let h = markdown_highlighted("- hello world", &[hl(vec![(2, 7)], 1)]);
        assert!(
            h.contains(r#"<li><mark class="add" data-n="1">hello</mark> world</li>"#),
            "{h}"
        );
    }

    #[test]
    fn highlight_inside_code_span() {
        let h = markdown_highlighted("say `a b` now", &[hl(vec![(5, 8)], 2)]);
        assert!(
            h.contains(r#"<code><mark class="add" data-n="2">a b</mark></code>"#),
            "{h}"
        );
    }

    #[test]
    fn highlights_on_two_lines_and_nested() {
        let md = "- first line\n- second line";
        let h = markdown_highlighted(md, &[hl(vec![(2, 12), (15, 21)], 1), hl(vec![(2, 7)], 2)]);
        assert_eq!(h.matches("<mark").count(), 3, "{h}");
        assert_eq!(h.matches("</mark>").count(), 3, "{h}");
    }

    #[test]
    fn pua_in_input_cannot_forge_marks() {
        let md = format!("x{}y{}z", '\u{E100}', '\u{E200}');
        let clean = strip_private_use(&md);
        assert_eq!(clean, "xyz");
        assert!(!markdown_highlighted(&clean, &[]).contains("<mark"));
    }

    #[test]
    fn raw_html_still_escaped_with_marks() {
        let md = "<b>x</b> text";
        let h = markdown_highlighted(md, &[hl(vec![(9, 13)], 1)]);
        assert!(!h.contains("<b>"), "{h}");
        assert!(h.contains("text</mark>"), "{h}");
    }

    #[test]
    fn highlight_of_whole_list_item_keeps_the_list() {
        let md = "- one\n- two whole item\n- three";
        let start = md.find("- two").unwrap();
        let h = markdown_highlighted(
            md,
            &[hl(vec![(start, start + "- two whole item".len())], 1)],
        );
        assert_eq!(h.matches("<li>").count(), 3, "{h}");
        assert!(
            h.contains(r#"<li><mark class="add" data-n="1">two whole item</mark></li>"#),
            "{h}"
        );
    }

    #[test]
    fn highlight_skips_heading_quote_and_numbered_prefixes() {
        for (md, inner) in [
            ("# Title here", "<h1>"),
            ("> quoted text", "<blockquote>"),
            ("1. first step", "<ol>"),
        ] {
            let h = markdown_highlighted(md, &[hl(vec![(0, md.len())], 1)]);
            assert!(h.contains(inner), "{md}: {h}");
            assert!(h.contains("<mark"), "{md}: {h}");
        }
    }

    #[test]
    fn locate_matches_rendered_text_without_markup() {
        // A selection in the rendered draft has no backticks or emphasis markers.
        let md = "- [practice] Add `x=1` to the **[Timer]** section\n- next";
        let spans = locate(md, "[practice] Add x=1 to the [Timer] section").unwrap();
        assert_eq!(
            &md[spans[0].0..spans[0].1],
            "[practice] Add `x=1` to the **[Timer]** section"
        );
    }

    #[test]
    fn locate_prefers_exact_match() {
        let md = "a `b` c and a b c";
        let spans = locate(md, "a b c").unwrap();
        assert_eq!(&md[spans[0].0..spans[0].1], "a b c");
    }
}
