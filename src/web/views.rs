//! View models and templates. Templates only format; all decisions live here.

use askama::Template;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::db::{self, MessageRow, ProposalRow, QueueFilter, QueueItem, SourceRow};
use crate::domain::{Action, Status, can_comment};
use crate::domain::{Anchor, ChangeKind};
use crate::i18n::Strings;
use crate::note::body;
use crate::render::{Highlight, diff, locate, markdown, markdown_highlighted, strip_private_use};

use super::AppState;

pub struct HeaderView {
    pub inbox: String,
    pub to_review: usize,
    pub with_agent: usize,
    pub untriaged: usize,
    pub agent_now: Option<String>,
    pub model: String,
}

pub struct QueueRow {
    pub id: i64,
    pub badge_class: &'static str,
    pub badge: String,
    pub title: String,
    pub meta: String,
    pub unsent: i64,
}

pub struct QueueView {
    pub filter: &'static str,
    pub rows: Vec<QueueRow>,
}

pub struct DiffLineView {
    pub class: &'static str,
    pub sign: &'static str,
    pub text: String,
    /// `sign + " " + text`: what a comment on this line quotes.
    pub key: String,
    pub comments: Vec<MsgView>,
}

pub struct DiffBlock {
    pub permalink: String,
    pub lines: Vec<DiffLineView>,
}

/// One entry in the margin: a change note from the agent or an anchored comment.
pub struct NoteView {
    pub n: usize,
    /// `add`, `chg`, `rm` or `cm`.
    pub class: &'static str,
    pub label: String,
    pub text: String,
    pub why: String,
    pub source: Option<String>,
    pub found: bool,
    pub is_comment: bool,
    pub version_tag: Option<i64>,
    pub unsent: bool,
}

pub struct SourceView {
    pub title: String,
    pub permalink: String,
    pub html: String,
}

pub struct MsgView {
    pub class: &'static str,
    pub who: &'static str,
    pub version: i64,
    pub time: String,
    pub html: String,
    pub unsent: bool,
    pub quote: Option<String>,
    pub model: Option<String>,
}

pub struct CardView {
    pub id: i64,
    pub version: i64,
    pub notes: Vec<NoteView>,
    pub badge_class: &'static str,
    pub badge: String,
    pub target_path: Option<String>,
    pub title: String,
    pub rationale: String,
    pub notice: Option<String>,
    pub hint: Option<String>,
    pub working: bool,
    pub error: Option<String>,
    pub has_draft: bool,
    pub diffs: Vec<DiffBlock>,
    pub draft_html: String,
    pub tags: Vec<String>,
    pub sources: Vec<SourceView>,
    pub thread: Vec<MsgView>,
    pub can_accept: bool,
    pub can_snooze: bool,
    pub can_regenerate: bool,
    pub can_retry: bool,
    pub can_comment: bool,
    pub can_send_any: bool,
}

#[derive(Template)]
#[template(path = "index.html")]
pub struct IndexPage<'a> {
    pub v: &'static str,
    pub t: &'a Strings,
    pub header: HeaderView,
    pub queue: QueueView,
    pub card: Option<CardView>,
}

#[derive(Template)]
#[template(path = "settings.html")]
pub struct SettingsPage<'a> {
    pub v: &'static str,
    pub t: &'a Strings,
    pub header: HeaderView,
    pub models: Vec<String>,
    pub current: String,
    pub endpoint: String,
    pub catalog_ok: bool,
    pub saved: bool,
    pub error: Option<String>,
}

#[derive(Template)]
#[template(path = "header.html")]
pub struct HeaderFragment<'a> {
    pub t: &'a Strings,
    pub header: HeaderView,
}

#[derive(Template)]
#[template(path = "queue.html")]
pub struct QueueFragment<'a> {
    pub t: &'a Strings,
    pub queue: QueueView,
}

#[derive(Template)]
#[template(path = "card.html")]
pub struct CardFragment<'a> {
    pub t: &'a Strings,
    pub card: Option<CardView>,
}

pub fn badge(
    t: &Strings,
    action: Action,
    status: Status,
    sources: usize,
) -> (&'static str, String) {
    match status {
        Status::AgentWorking => ("t-agent", t.badge_agent.into()),
        Status::Snoozed => ("t-muted", t.badge_snoozed.into()),
        Status::Stale => ("t-warn", t.badge_stale.into()),
        Status::Closed => ("t-muted", t.badge_closed.into()),
        Status::Applying => ("t-warn", t.badge_applying.into()),
        Status::Accepted => ("t-muted", t.badge_accepted.into()),
        Status::Ready => match action {
            Action::Promote => ("t-promote", t.badge_promote.into()),
            Action::Merge => ("t-merge", format!("{} ×{sources}", t.badge_merge)),
            Action::Delete => ("t-delete", t.badge_delete.into()),
        },
    }
}

fn short_date(t: OffsetDateTime) -> String {
    format!("{:02}.{:02}", t.day(), u8::from(t.month()))
}

pub fn queue_row(t: &Strings, item: &QueueItem, verified_dir: &str) -> QueueRow {
    let (badge_class, badge) = badge(t, item.action, item.status, item.sources_count as usize);
    let place = match (&item.target_dir, item.action) {
        (Some(d), Action::Promote | Action::Merge) => format!("{verified_dir}/{d}"),
        _ => format!("{} {}", item.sources_count, t.sources_n),
    };
    QueueRow {
        id: item.id,
        badge_class,
        badge,
        title: item.title.clone(),
        meta: format!("{place} · {}", short_date(item.updated_at)),
        unsent: item.unsent,
    }
}

pub fn parse_filter(f: Option<&str>) -> (&'static str, QueueFilter) {
    match f {
        Some("all") => ("all", QueueFilter::All),
        _ => ("open", QueueFilter::Open),
    }
}

pub async fn queue_view(s: &AppState, filter: Option<&str>) -> anyhow::Result<QueueView> {
    let (name, f) = parse_filter(filter);
    let items = s.db.call(move |c| db::list_queue(c, f)).await?;
    Ok(QueueView {
        filter: name,
        rows: items
            .iter()
            .map(|i| queue_row(s.t, i, &s.cfg.verified_dir))
            .collect(),
    })
}

pub async fn header_view(s: &AppState) -> anyhow::Result<HeaderView> {
    let open = s.db.call(|c| db::list_queue(c, QueueFilter::Open)).await?;
    let to_review = open.iter().filter(|i| i.status == Status::Ready).count();
    let working = open
        .iter()
        .filter(|i| i.status == Status::AgentWorking)
        .count();
    let (queued, agent_now) = {
        let st = s.agent.state.lock().unwrap_or_else(|p| p.into_inner());
        (st.queued_triage.len(), st.current.clone())
    };
    // The header must render even when the memory server is down.
    let (inbox, untriaged) =
        match crate::agent::untriaged(s.memory.as_ref(), &s.db, &s.cfg.inbox_dir).await {
            Ok(free) => {
                let claimed = s.db.call(|c| db::claimed_permalinks(c)).await?.len();
                (
                    (free.len() + claimed).to_string(),
                    free.len().saturating_sub(queued),
                )
            }
            Err(e) => {
                tracing::warn!("listing the inbox failed: {e:#}");
                ("—".to_string(), 0)
            }
        };
    Ok(HeaderView {
        model: crate::llm::current_model(&s.model),
        inbox,
        to_review,
        with_agent: working + queued,
        untriaged,
        agent_now,
    })
}

fn message_view(t: &Strings, m: &MessageRow) -> MsgView {
    let (class, who) = match m.author.as_str() {
        "agent" => ("agent", t.agent_says),
        "system" => ("system", t.system_says),
        _ => ("human", t.you_say),
    };
    MsgView {
        class,
        who,
        version: m.draft_version,
        time: m.created_at.format(&Rfc3339).unwrap_or_default(),
        html: markdown(&m.body),
        unsent: !m.sent,
        quote: match &m.anchor {
            Some(Anchor::Draft { quote, .. }) => Some(quote.clone()),
            Some(Anchor::Diff { line, .. }) => Some(line.clone()),
            None => None,
        },
        model: m.model.clone(),
    }
}

/// Margin notes in display order and the draft HTML with their highlights.
fn annotate(
    t: &Strings,
    p: &ProposalRow,
    draft: &str,
    thread: &[MessageRow],
) -> (Vec<NoteView>, String) {
    struct Item {
        pos: Option<usize>,
        spans: Vec<crate::render::Span>,
        note: NoteView,
    }
    let clean = strip_private_use(draft);
    let mut items: Vec<Item> = Vec::new();
    for c in &p.changes {
        let (class, label) = match c.kind {
            ChangeKind::Added => ("add", t.note_added),
            ChangeKind::Rewritten => ("chg", t.note_rewritten),
            ChangeKind::Removed => ("rm", t.note_removed),
        };
        let spans = match c.kind {
            ChangeKind::Removed => None,
            _ => locate(&clean, &c.text),
        };
        items.push(Item {
            pos: spans.as_ref().map(|s| s[0].0),
            note: NoteView {
                n: 0,
                class,
                label: label.to_string(),
                text: c.text.clone(),
                why: c.why.clone(),
                source: c.source.clone(),
                found: spans.is_some(),
                is_comment: false,
                version_tag: None,
                unsent: false,
            },
            spans: spans.unwrap_or_default(),
        });
    }
    for m in thread {
        let Some(Anchor::Draft { version, quote }) = &m.anchor else {
            continue;
        };
        let spans = if *version == p.version {
            locate(&clean, quote)
        } else {
            None
        };
        items.push(Item {
            pos: spans.as_ref().map(|s| s[0].0),
            note: NoteView {
                n: 0,
                class: "cm",
                label: t.note_comment.to_string(),
                text: quote.clone(),
                why: m.body.clone(),
                source: None,
                found: spans.is_some(),
                is_comment: true,
                version_tag: (*version != p.version).then_some(*version),
                unsent: !m.sent,
            },
            spans: spans.unwrap_or_default(),
        });
    }
    // Located items by position; then unlocated changes, then unlocated comments.
    let rank = |i: &Item| match (i.pos, i.note.is_comment) {
        (Some(_), _) => 0,
        (None, false) => 1,
        (None, true) => 2,
    };
    items.sort_by_key(|i| (rank(i), i.pos.unwrap_or(usize::MAX)));
    let mut highlights = Vec::new();
    for (k, item) in items.iter_mut().enumerate() {
        item.note.n = k + 1;
        if !item.spans.is_empty() {
            highlights.push(Highlight {
                spans: item.spans.clone(),
                open: format!(
                    r#"<mark class="{}" data-n="{}">"#,
                    item.note.class, item.note.n
                ),
            });
        }
    }
    let html = markdown_highlighted(&clean, &highlights);
    (items.into_iter().map(|i| i.note).collect(), html)
}

pub fn card_view(
    t: &Strings,
    verified_dir: &str,
    p: &ProposalRow,
    sources: &[SourceRow],
    thread: &[MessageRow],
    notice: Option<String>,
) -> CardView {
    let (badge_class, badge) = badge(t, p.action, p.status, sources.len());
    let draft = p.draft.clone().unwrap_or_default();
    let has_draft = p.draft.is_some() && p.action != Action::Delete;
    let hint = match p.status {
        Status::AgentWorking => Some(t.agent_working.to_string()),
        Status::Stale => Some(t.stale_hint.to_string()),
        Status::Closed => Some(t.closed_hint.to_string()),
        Status::Applying => Some(t.applying_hint.to_string()),
        Status::Accepted => Some(t.accepted_hint.to_string()),
        Status::Snoozed => p
            .snoozed_until
            .map(|u| format!("{} {}", t.snoozed_until, short_date(u))),
        Status::Ready => None,
    };
    let (notes, draft_html) = if has_draft {
        annotate(t, p, &draft, thread)
    } else {
        (Vec::new(), String::new())
    };
    let line_comments = |permalink: &str, key: &str| -> Vec<MsgView> {
        thread
            .iter()
            .filter(|m| matches!(&m.anchor, Some(Anchor::Diff { permalink: pl, line }) if pl == permalink && line == key))
            .map(|m| message_view(t, m))
            .collect()
    };
    CardView {
        id: p.id,
        version: p.version,
        notes,
        badge_class,
        badge,
        target_path: match (&p.target_dir, &p.target_title) {
            (Some(d), Some(title)) if has_draft => Some(format!("{verified_dir}/{d}/{title}")),
            _ => None,
        },
        title: p
            .target_title
            .clone()
            .or_else(|| sources.first().map(|s| s.title.clone()))
            .unwrap_or_default(),
        rationale: p.rationale.clone(),
        notice,
        hint,
        working: p.status == Status::AgentWorking,
        error: p.error.clone(),
        has_draft,
        diffs: if has_draft {
            sources
                .iter()
                .map(|s| DiffBlock {
                    permalink: s.permalink.clone(),
                    lines: diff(&s.original, &draft)
                        .into_iter()
                        .map(|l| {
                            let key = format!("{} {}", l.sign(), l.text);
                            DiffLineView {
                                class: l.class(),
                                sign: l.sign(),
                                comments: line_comments(&s.permalink, &key),
                                key,
                                text: l.text,
                            }
                        })
                        .collect(),
                })
                .collect()
        } else {
            Vec::new()
        },
        draft_html,
        tags: p.tags.clone(),
        sources: sources
            .iter()
            .map(|s| SourceView {
                title: s.title.clone(),
                permalink: s.permalink.clone(),
                html: markdown(body(&s.original)),
            })
            .collect(),
        thread: thread.iter().map(|m| message_view(t, m)).collect(),
        can_accept: p.status == Status::Ready,
        can_snooze: p.status == Status::Ready,
        can_regenerate: p.status == Status::Stale,
        can_retry: p.status == Status::Applying,
        can_comment: can_comment(p.status),
        can_send_any: p.status == Status::Ready,
    }
}

/// Loads and builds a card, or `None` if it does not exist.
pub async fn load_card(
    s: &AppState,
    id: i64,
    notice: Option<String>,
) -> anyhow::Result<Option<CardView>> {
    let loaded =
        s.db.call(move |c| {
            let Some(p) = db::get_proposal(c, id)? else {
                return Ok(None);
            };
            Ok(Some((p, db::sources(c, id)?, db::messages(c, id)?)))
        })
        .await?;
    Ok(loaded.map(|(p, src, msgs)| card_view(s.t, &s.cfg.verified_dir, &p, &src, &msgs, notice)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Lang;
    use crate::domain::{Anchor, Change, ChangeKind};
    use crate::i18n::strings;

    fn proposal(draft: &str, changes: Vec<Change>, version: i64) -> ProposalRow {
        ProposalRow {
            id: 1,
            action: Action::Promote,
            status: Status::Ready,
            target_dir: Some("ops".into()),
            target_title: Some("T".into()),
            draft: Some(draft.into()),
            tags: vec![],
            rationale: "r".into(),
            version,
            snoozed_until: None,
            error: None,
            applied_permalink: None,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            changes,
        }
    }

    fn change(kind: ChangeKind, text: &str) -> Change {
        Change {
            kind,
            text: text.into(),
            source: Some("p/inbox/a".into()),
            why: format!("why {text}"),
        }
    }

    fn comment(quote: &str, version: i64) -> MessageRow {
        MessageRow {
            id: 9,
            author: "human".into(),
            body: "note".into(),
            draft_version: version,
            sent: false,
            created_at: OffsetDateTime::UNIX_EPOCH,
            anchor: Some(Anchor::Draft {
                version,
                quote: quote.into(),
            }),
            model: None,
        }
    }

    fn src() -> Vec<SourceRow> {
        vec![SourceRow {
            permalink: "p/inbox/a".into(),
            title: "A".into(),
            content_hash: "h".into(),
            original: "- old line".into(),
        }]
    }

    #[test]
    fn notes_numbered_by_position_then_removed() {
        let draft = "- first fact here\n- second fact here";
        let p = proposal(
            draft,
            vec![
                change(ChangeKind::Removed, "old line"),
                change(ChangeKind::Added, "second fact"),
                change(ChangeKind::Rewritten, "first fact"),
            ],
            1,
        );
        let c = card_view(strings(Lang::En), "verified", &p, &src(), &[], None);
        let order: Vec<(usize, &str)> = c.notes.iter().map(|n| (n.n, n.class)).collect();
        assert_eq!(order, [(1, "chg"), (2, "add"), (3, "rm")]);
        assert!(
            c.draft_html
                .contains(r#"<mark class="chg" data-n="1">first fact</mark>"#),
            "{}",
            c.draft_html
        );
        assert!(c.draft_html.contains(r#"data-n="2">second fact</mark>"#));
    }

    #[test]
    fn unfound_change_marked_not_found() {
        let p = proposal(
            "- text",
            vec![change(ChangeKind::Added, "nowhere to be seen")],
            1,
        );
        let c = card_view(strings(Lang::En), "verified", &p, &src(), &[], None);
        assert_eq!(c.notes.len(), 1);
        assert!(!c.notes[0].found);
        assert!(!c.draft_html.contains("<mark"));
    }

    #[test]
    fn comment_on_current_version_highlighted_old_version_tagged() {
        let p = proposal("- alpha beta gamma", vec![], 2);
        let msgs = [comment("beta gamma", 2), comment("alpha", 1)];
        let c = card_view(strings(Lang::En), "verified", &p, &src(), &msgs, None);
        assert_eq!(c.notes.len(), 2);
        assert!(c.notes[0].is_comment && c.notes[0].found && c.notes[0].class == "cm");
        assert_eq!(c.notes[1].version_tag, Some(1));
        assert!(!c.notes[1].found);
        assert!(
            c.draft_html
                .contains(r#"<mark class="cm" data-n="1">beta gamma</mark>"#),
            "{}",
            c.draft_html
        );
        assert_eq!(c.thread[0].quote.as_deref(), Some("beta gamma"));
    }

    #[test]
    fn diff_comment_attached_to_its_line() {
        let p = proposal("- new line", vec![], 1);
        let m = MessageRow {
            anchor: Some(Anchor::Diff {
                permalink: "p/inbox/a".into(),
                line: "- - old line".into(),
            }),
            ..comment("x", 1)
        };
        let c = card_view(strings(Lang::En), "verified", &p, &src(), &[m], None);
        let line = c.diffs[0]
            .lines
            .iter()
            .find(|l| l.key == "- - old line")
            .unwrap();
        assert_eq!(line.comments.len(), 1);
    }

    #[test]
    fn agent_message_shows_model() {
        let p = proposal("- x", vec![], 1);
        let m = MessageRow {
            author: "agent".into(),
            anchor: None,
            model: Some("m-2".into()),
            ..comment("x", 1)
        };
        let c = card_view(strings(Lang::En), "verified", &p, &src(), &[m], None);
        assert_eq!(c.thread[0].model.as_deref(), Some("m-2"));
    }
}
