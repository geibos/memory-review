//! View models and templates. Templates only format; all decisions live here.

use askama::Template;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::db::{self, MessageRow, ProposalRow, QueueFilter, QueueItem, SourceRow};
use crate::domain::{Action, Status, can_comment};
use crate::i18n::Strings;
use crate::note::body;
use crate::render::{DiffLine, diff, markdown};

use super::AppState;

pub struct HeaderView {
    pub inbox: String,
    pub to_review: usize,
    pub with_agent: usize,
    pub untriaged: usize,
    pub agent_now: Option<String>,
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

pub struct DiffBlock {
    pub permalink: String,
    pub lines: Vec<DiffLine>,
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
}

pub struct CardView {
    pub id: i64,
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
    pub t: &'a Strings,
    pub header: HeaderView,
    pub queue: QueueView,
    pub card: Option<CardView>,
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
    }
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
    CardView {
        id: p.id,
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
                    lines: diff(&s.original, &draft),
                })
                .collect()
        } else {
            Vec::new()
        },
        draft_html: markdown(&draft),
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
