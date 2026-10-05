//! Prompt templates: built in, optionally overridden from a directory.

use std::path::Path;

pub struct Prompts {
    pub system: String,
    pub triage: String,
    pub reply: String,
}

const SYSTEM: &str = include_str!("../prompts/system.md");
const TRIAGE: &str = include_str!("../prompts/triage.md");
const REPLY: &str = include_str!("../prompts/reply.md");

impl Prompts {
    /// Built-in prompts, with any of `system.md`, `triage.md`, `reply.md`
    /// found in `dir` taking precedence.
    ///
    /// Synchronous file I/O: call once at startup, before serving requests.
    pub fn load(dir: Option<&Path>) -> anyhow::Result<Prompts> {
        let pick = |name: &str, builtin: &str| -> anyhow::Result<String> {
            match dir.map(|d| d.join(name)) {
                Some(path) if path.exists() => std::fs::read_to_string(&path)
                    .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display())),
                _ => Ok(builtin.to_string()),
            }
        };
        Ok(Prompts {
            system: pick("system.md", SYSTEM)?,
            triage: pick("triage.md", TRIAGE)?,
            reply: pick("reply.md", REPLY)?,
        })
    }
}

/// Replaces every `{{key}}` with its value. Values are inserted verbatim and
/// are not scanned again, so a note containing `{{x}}` cannot inject a slot.
pub fn render(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let key = &after[..end];
                match vars.iter().find(|(k, _)| *k == key) {
                    Some((_, v)) => out.push_str(v),
                    None => out.push_str(&rest[start..start + 2 + end + 2]),
                }
                rest = &after[end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_replaces_all_occurrences() {
        assert_eq!(
            render("{{a}} and {{a}} / {{b}}", &[("a", "1"), ("b", "2")]),
            "1 and 1 / 2"
        );
    }

    #[test]
    fn render_does_not_expand_inside_values() {
        assert_eq!(
            render("{{a}}{{b}}", &[("a", "{{b}}"), ("b", "x")]),
            "{{b}}x"
        );
    }

    #[test]
    fn unknown_slots_left_as_is() {
        assert_eq!(render("{{zzz}}", &[("a", "1")]), "{{zzz}}");
    }

    #[test]
    fn embedded_prompts_have_placeholders() {
        let p = Prompts::load(None).unwrap();
        for slot in [
            "{{origin}}",
            "{{origin_permalink}}",
            "{{candidates}}",
            "{{verified}}",
            "{{verified_dirs}}",
        ] {
            assert!(p.triage.contains(slot), "triage lacks {slot}");
        }
        for slot in [
            "{{draft}}",
            "{{thread}}",
            "{{pending}}",
            "{{sources}}",
            "{{candidates}}",
        ] {
            assert!(p.reply.contains(slot), "reply lacks {slot}");
        }
        assert!(p.system.contains("tool"));
    }

    #[test]
    fn directory_overrides_only_present_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("system.md"), "custom system").unwrap();
        let p = Prompts::load(Some(dir.path())).unwrap();
        assert_eq!(p.system, "custom system");
        assert_eq!(p.triage, TRIAGE);
    }
}
