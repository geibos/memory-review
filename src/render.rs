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

fn options() -> pulldown_cmark::Options {
    use pulldown_cmark::Options;
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS
}

/// Raw HTML becomes text; link and image URLs with a non-web scheme become `#`.
fn sanitize(ev: pulldown_cmark::Event<'_>) -> pulldown_cmark::Event<'_> {
    use pulldown_cmark::{CowStr, Event, Tag};

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

    match ev {
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
    }
}

/// Markdown to HTML. Raw HTML in the input is shown as text, never rendered.
pub fn markdown(md: &str) -> String {
    let mut out = String::new();
    pulldown_cmark::html::push_html(
        &mut out,
        pulldown_cmark::Parser::new_ext(md, options()).map(sanitize),
    );
    out
}

/// Byte range in the markdown source.
pub type Span = (usize, usize);

/// One highlight: where it is, its CSS class and its number.
pub struct Highlight {
    pub spans: Vec<Span>,
    pub class: &'static str,
    pub n: usize,
}

impl Highlight {
    fn open(&self, last: bool) -> String {
        let more = if last { "" } else { " more" };
        format!(r#"<mark class="{}{more}" data-n="{}">"#, self.class, self.n)
    }
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

/// Markdown to HTML with the given spans wrapped in `<mark>` tags.
///
/// The source is never modified: tags are placed around the parser's text
/// events by their source offsets, so block syntax (lists, tables, fences,
/// headings) renders exactly as without highlights. Text inside images (the
/// alt attribute) is never wrapped. A highlight spread over several pieces of
/// text shows its number badge only on the last piece (`more` on the others).
pub fn markdown_highlighted(md: &str, highlights: &[Highlight]) -> String {
    use pulldown_cmark::{CowStr, Event, Parser, Tag, TagEnd};

    let last_end: Vec<usize> = highlights
        .iter()
        .map(|h| h.spans.iter().map(|s| s.1).max().unwrap_or(0))
        .collect();
    // Highlights with a span that fully covers [a, b).
    let covering = |a: usize, b: usize| -> Vec<usize> {
        (0..highlights.len())
            .filter(|&i| highlights[i].spans.iter().any(|&(s, e)| s <= a && b <= e))
            .collect()
    };
    // Highlights with a span overlapping [a, b) at all.
    let touching = |a: usize, b: usize| -> Vec<usize> {
        (0..highlights.len())
            .filter(|&i| highlights[i].spans.iter().any(|&(s, e)| s < b && a < e))
            .collect()
    };
    fn wrap<'a>(
        out: &mut Vec<Event<'a>>,
        highlights: &[Highlight],
        last_end: &[usize],
        cover: &[usize],
        end: usize,
        inner: Event<'a>,
    ) {
        for &i in cover {
            out.push(Event::InlineHtml(CowStr::from(
                highlights[i].open(end >= last_end[i]),
            )));
        }
        out.push(inner);
        for _ in cover {
            out.push(Event::InlineHtml(CowStr::Borrowed("</mark>")));
        }
    }

    let mut events: Vec<Event<'_>> = Vec::new();
    let mut in_image = 0usize;
    for (ev, range) in Parser::new_ext(md, options()).into_offset_iter() {
        let ev = sanitize(ev);
        match &ev {
            Event::Start(Tag::Image { .. }) => in_image += 1,
            Event::End(TagEnd::Image) => in_image = in_image.saturating_sub(1),
            _ => {}
        }
        if in_image > 0 || highlights.is_empty() {
            events.push(ev);
            continue;
        }
        match ev {
            // Text that is a verbatim slice of the source: split it exactly.
            Event::Text(t) if md.get(range.clone()) == Some(t.as_ref()) => {
                let mut cuts = vec![range.start, range.end];
                for h in highlights {
                    for &(s, e) in &h.spans {
                        for p in [s, e] {
                            if p > range.start && p < range.end && md.is_char_boundary(p) {
                                cuts.push(p);
                            }
                        }
                    }
                }
                cuts.sort_unstable();
                cuts.dedup();
                for w in cuts.windows(2) {
                    let (a, b) = (w[0], w[1]);
                    let piece = Event::Text(CowStr::Borrowed(&md[a..b]));
                    let cover = covering(a, b);
                    if cover.is_empty() {
                        events.push(piece);
                    } else {
                        wrap(&mut events, highlights, &last_end, &cover, b, piece);
                    }
                }
            }
            // Escaped text or inline code: wrap the whole piece if touched.
            ev @ (Event::Text(_) | Event::Code(_)) => {
                let cover = touching(range.start, range.end);
                if cover.is_empty() {
                    events.push(ev);
                } else {
                    wrap(&mut events, highlights, &last_end, &cover, range.end, ev);
                }
            }
            other => events.push(other),
        }
    }
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, events.into_iter());
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
            class: "add",
            n,
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
            h.contains(r#"<mark class="add" data-n="2"><code>a b</code></mark>"#),
            "{h}"
        );
    }

    #[test]
    fn highlights_on_two_lines_and_nested() {
        let md = "- first line\n- second line";
        let h = markdown_highlighted(md, &[hl(vec![(2, 12), (15, 21)], 1), hl(vec![(2, 7)], 2)]);
        // Segments: "first" (both), " line" (1), "second" (1).
        assert_eq!(h.matches("<mark").count(), 4, "{h}");
        assert_eq!(h.matches("</mark>").count(), 4, "{h}");
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

    fn whole(md: &str, from: &str, to_end: &str) -> Vec<Span> {
        let a = md.find(from).unwrap();
        let b = md.find(to_end).unwrap() + to_end.len();
        vec![(a, b)]
    }

    #[test]
    fn table_row_highlight_keeps_table() {
        let md = "| a | b |\n|---|---|\n| ячейка | два |";
        let h = markdown_highlighted(md, &[hl(whole(md, "| ячейка", "два |"), 1)]);
        assert_eq!(h.matches("<td>").count(), 2, "{h}");
        assert!(h.contains("два"), "{h}");
        assert!(h.contains("<mark"), "{h}");
    }

    #[test]
    fn fenced_code_highlight_keeps_fence() {
        let md = "intro\n```\nкод\n```\nafter *x*";
        let h = markdown_highlighted(md, &[hl(whole(md, "```\nкод", "код"), 1)]);
        assert_eq!(h.matches("<pre>").count(), 1, "{h}");
        assert!(h.contains("<em>x</em>"), "{h}");
    }

    #[test]
    fn setext_heading_and_rule_kept() {
        let md = "Title\n---\n\n* * *\nnext";
        let h = markdown_highlighted(
            md,
            &[hl(vec![(0, 9)], 1), hl(whole(md, "* * *", "* * *"), 2)],
        );
        assert!(h.contains("<h2>"), "{h}");
        assert!(h.contains("<hr />"), "{h}");
    }

    #[test]
    fn task_checkbox_kept() {
        let md = "- [ ] задача";
        let h = markdown_highlighted(md, &[hl(vec![(0, md.len())], 1)]);
        assert!(h.contains(r#"type="checkbox""#), "{h}");
        assert!(h.contains("задача</mark>"), "{h}");
    }

    #[test]
    fn image_alt_and_link_url_untouched() {
        let md = "![картинка](https://example.com/a.png) and [link](https://example.com)";
        let spans = vec![(0, md.len())];
        let h = markdown_highlighted(md, &[hl(spans, 1)]);
        assert!(h.contains(r#"alt="картинка""#), "{h}");
        assert!(h.contains(r#"href="https://example.com""#), "{h}");
    }

    #[test]
    fn number_badge_once_per_highlight() {
        let md = "- one two\n- three four";
        let h = markdown_highlighted(md, &[hl(vec![(2, 9), (12, 22)], 1)]);
        let finals = h.matches(r#"<mark class="add" data-n="1">"#).count();
        let more = h.matches(r#"<mark class="add more" data-n="1">"#).count();
        assert_eq!((finals, more), (1, 1), "{h}");
    }
}
