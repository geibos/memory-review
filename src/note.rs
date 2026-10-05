//! Plain-text helpers for Basic Memory notes: frontmatter, body, hashing.

use sha2::{Digest, Sha256};

/// A note as stored on disk, read back through `read_content`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawNote {
    pub permalink: String,
    pub title: String,
    pub raw: String,
}

/// Splits off the YAML frontmatter: `(block without fences, rest of the note)`.
fn frontmatter(raw: &str) -> Option<(&str, &str)> {
    let rest = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))?;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

/// Value of a top-level, single-line frontmatter field, with surrounding quotes removed.
pub fn frontmatter_field(raw: &str, key: &str) -> Option<String> {
    let (fm, _) = frontmatter(raw)?;
    fm.lines().find_map(|line| {
        if line.starts_with([' ', '\t', '-']) {
            return None;
        }
        let (k, v) = line.split_once(':')?;
        (k.trim() == key).then(|| unquote(v.trim()))
    })
}

fn unquote(v: &str) -> String {
    if let Some(inner) = v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
        inner.replace("''", "'")
    } else if let Some(inner) = v.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        inner.replace("\\\"", "\"")
    } else {
        v.to_string()
    }
}

/// Note text without the frontmatter block.
pub fn body(raw: &str) -> &str {
    frontmatter(raw).map_or(raw, |(_, rest)| rest.trim_start())
}

/// Hex SHA-256 of the raw note text.
pub fn content_hash(raw: &str) -> String {
    hex::encode(Sha256::digest(raw.as_bytes()))
}

/// Folder a permalink lives in: `project/inbox/slug` → `inbox`.
pub fn folder_of(permalink: &str) -> Option<&str> {
    permalink.split('/').nth(1)
}

/// Whether a stored note's body equals a draft, ignoring frontmatter, a
/// leading `# Title` heading and whitespace differences.
pub fn same_body(stored_raw: &str, draft: &str) -> bool {
    normalize(body(stored_raw)) == normalize(draft)
}

fn normalize(text: &str) -> String {
    let text = text.replace("\r\n", "\n");
    let text = text.trim();
    let text = match text.split_once('\n') {
        Some((first, rest)) if first.starts_with("# ") => rest.trim_start(),
        None if text.starts_with("# ") => "",
        _ => text,
    };
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0;
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: &str = "---\ntitle: 'A: b'\ntype: note\npermalink: proj/inbox/a-b\ntags:\n- x\n---\n\n# A: b\n\n- [fact] one\n";

    #[test]
    fn reads_quoted_title_and_permalink() {
        assert_eq!(frontmatter_field(N, "title").as_deref(), Some("A: b"));
        assert_eq!(
            frontmatter_field(N, "permalink").as_deref(),
            Some("proj/inbox/a-b")
        );
        assert_eq!(frontmatter_field(N, "missing"), None);
    }

    #[test]
    fn nested_list_items_are_not_fields() {
        assert_eq!(frontmatter_field(N, "- x"), None);
        assert_eq!(frontmatter_field(N, "tags").as_deref(), Some(""));
    }

    #[test]
    fn body_strips_frontmatter() {
        assert!(body(N).starts_with("# A: b"));
        assert_eq!(body("no fm"), "no fm");
        assert_eq!(body("---\nunterminated"), "---\nunterminated");
    }

    #[test]
    fn hash_is_stable_hex() {
        assert_eq!(content_hash("x"), content_hash("x"));
        assert_ne!(content_hash("x"), content_hash("y"));
        assert_eq!(content_hash("x").len(), 64);
    }

    #[test]
    fn folder_is_second_segment() {
        assert_eq!(folder_of("p/inbox/a"), Some("inbox"));
        assert_eq!(folder_of("p/verified/sub/a"), Some("verified"));
        assert_eq!(folder_of("a"), None);
    }

    #[test]
    fn same_body_ignores_frontmatter_title_heading_and_trailing_ws() {
        assert!(same_body(N, "- [fact] one"));
        assert!(same_body(N, "# A: b\n\n- [fact] one\n\n"));
        assert!(same_body(N, "- [fact] one\r\n"));
        assert!(!same_body(N, "- [fact] two"));
    }

    #[test]
    fn frontmatter_value_with_colons_kept() {
        let n = "---\ntitle: \"x: y: z\"\n---\nb";
        assert_eq!(frontmatter_field(n, "title").as_deref(), Some("x: y: z"));
    }

    #[test]
    fn crlf_frontmatter_supported() {
        let n = "---\r\ntitle: T\r\npermalink: p/inbox/t\r\n---\r\nbody";
        assert_eq!(
            frontmatter_field(n, "permalink").as_deref(),
            Some("p/inbox/t")
        );
        assert_eq!(body(n), "body");
    }
}
