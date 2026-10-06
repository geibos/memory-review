//! The agent: prepares proposals for inbox notes and answers comments.
//!
//! It only reads memory; nothing here writes to the vault. Jobs run one at a
//! time on a single worker task and start only from user actions.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};

use crate::db::{self, ClaimConflict, Db, SourceRow};
use crate::domain::{AgentProposal, Anchor, ClaimContext, Event, Status, ValidatedProposal};
use crate::llm::{ChatMessage, Llm, LlmOutcome, ToolSpec};
use crate::memory::{InboxEntry, MemoryApi, inbox_permalinks};
use crate::note::{RawNote, body, content_hash, folder_of};
use crate::prompts::{Prompts, render};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    Triage { permalink: String },
    Reply { id: i64 },
    Regenerate { id: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    Queued,
    Started(String),
    Finished(String),
    Failed(String),
}

#[derive(Debug, Default)]
pub struct AgentState {
    pub queued_triage: HashSet<String>,
    pub queued: usize,
    pub current: Option<String>,
}

#[derive(Clone)]
pub struct AgentHandle {
    tx: mpsc::Sender<Job>,
    pub events: broadcast::Sender<AgentEvent>,
    pub state: Arc<Mutex<AgentState>>,
}

#[derive(Debug, Clone)]
pub struct AgentCfg {
    pub inbox_dir: String,
    pub verified_dir: String,
}

pub struct Agent {
    pub memory: Arc<dyn MemoryApi>,
    pub llm: Arc<dyn Llm>,
    pub db: Db,
    pub prompts: Arc<Prompts>,
    pub cfg: AgentCfg,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriageResult {
    Created(i64),
    SkippedClaimed,
    SkippedMissing,
}

/// Queue capacity. A full queue refuses new jobs instead of growing without bound.
const QUEUE: usize = 1024;
/// How much of a note goes into a prompt.
const ORIGIN_MAX: usize = 20_000;
const CANDIDATE_MAX: usize = 6_000;
const MAX_INBOX_CANDIDATES: usize = 4;
const MAX_VERIFIED_CANDIDATES: usize = 3;
const MAX_LISTED_FREE: usize = 30;

fn lock(state: &Mutex<AgentState>) -> std::sync::MutexGuard<'_, AgentState> {
    // State is plain bookkeeping; a panic elsewhere must not wedge the queue.
    state.lock().unwrap_or_else(|p| p.into_inner())
}

impl AgentHandle {
    /// Queues a job; returns `false` if it was a duplicate triage or the queue is full.
    pub fn enqueue(&self, job: Job) -> bool {
        {
            let mut st = lock(&self.state);
            if let Job::Triage { permalink } = &job
                && !st.queued_triage.insert(permalink.clone())
            {
                return false;
            }
            st.queued += 1;
        }
        match self.tx.try_send(job) {
            Ok(()) => {
                let _ = self.events.send(AgentEvent::Queued);
                true
            }
            Err(e) => {
                let job = match e {
                    mpsc::error::TrySendError::Full(j) | mpsc::error::TrySendError::Closed(j) => j,
                };
                tracing::warn!(?job, "agent queue refused a job");
                let mut st = lock(&self.state);
                st.queued = st.queued.saturating_sub(1);
                if let Job::Triage { permalink } = &job {
                    st.queued_triage.remove(permalink);
                }
                false
            }
        }
    }
}

/// Inbox notes not held by any open card.
///
/// cancel-safe: yes — read-only.
pub async fn untriaged(
    memory: &dyn MemoryApi,
    db: &Db,
    inbox_dir: &str,
) -> anyhow::Result<Vec<InboxEntry>> {
    let inbox = memory.list_dir(inbox_dir).await?;
    let claimed = db.call(|c| db::claimed_permalinks(c)).await?;
    Ok(inbox
        .into_iter()
        .filter(|e| !claimed.contains(&e.permalink))
        .collect())
}

/// How a human comment is shown to the model, with what it points at.
pub fn format_comment(m: &db::MessageRow) -> String {
    match &m.anchor {
        None => m.body.clone(),
        Some(Anchor::Draft { version, quote }) => {
            format!("Comment on draft v{version} fragment “{quote}”: {}", m.body)
        }
        Some(Anchor::Diff { permalink, line }) => {
            format!(
                "Comment on diff line of `{permalink}`: “{line}”: {}",
                m.body
            )
        }
    }
}

fn changes_schema() -> Value {
    json!({
        "type": "array",
        "maxItems": crate::domain::MAX_CHANGES,
        "description": "Every meaningful change against the source notes (not formatting).",
        "items": {
            "type": "object",
            "properties": {
                "kind": { "type": "string", "enum": ["added", "rewritten", "removed"] },
                "text": { "type": "string", "description": "Exact quote: from the draft for added/rewritten, from a source for removed." },
                "source": { "type": "string", "description": "Permalink of the source note; required for removed." },
                "why": { "type": "string", "description": "One sentence for the reviewer." }
            },
            "required": ["kind", "text", "why"]
        }
    })
}

fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}\n…[truncated]", &s[..i]),
        None => s.to_string(),
    }
}

fn source_row(n: &RawNote) -> SourceRow {
    SourceRow {
        permalink: n.permalink.clone(),
        title: n.title.clone(),
        content_hash: content_hash(&n.raw),
        original: n.raw.clone(),
    }
}

fn proposal_tool() -> ToolSpec {
    ToolSpec {
        name: "submit_proposal",
        description: "Submit the review proposal for the inbox note.",
        parameters: json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["promote", "merge", "delete"] },
                "sources": { "type": "array", "items": { "type": "string" },
                             "description": "Permalinks of the inbox notes this proposal consumes." },
                "target_dir": { "type": "string", "description": "Single folder name under verified/." },
                "target_title": { "type": "string" },
                "draft": { "type": "string", "description": "Full markdown of the verified note, without frontmatter." },
                "tags": { "type": "array", "items": { "type": "string" } },
                "rationale": { "type": "string", "description": "1-3 sentences for the reviewer." },
                "changes": changes_schema()
            },
            "required": ["action", "sources", "rationale"]
        }),
    }
}

fn reply_tool() -> ToolSpec {
    ToolSpec {
        name: "submit_reply",
        description: "Answer the reviewer and optionally revise the proposal.",
        parameters: json!({
            "type": "object",
            "properties": {
                "reply": { "type": "string" },
                "action": { "type": "string", "enum": ["promote", "merge", "delete"] },
                "sources": { "type": "array", "items": { "type": "string" } },
                "target_dir": { "type": "string" },
                "target_title": { "type": "string" },
                "draft": { "type": "string" },
                "tags": { "type": "array", "items": { "type": "string" } },
                "rationale": { "type": "string", "description": "Updated 1-3 sentence rationale when the proposal changes." },
                "changes": changes_schema()
            },
            "required": ["reply"]
        }),
    }
}

#[derive(Debug, Deserialize)]
struct ReplyArgs {
    reply: String,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    sources: Option<Vec<String>>,
    #[serde(default)]
    target_dir: Option<String>,
    #[serde(default)]
    target_title: Option<String>,
    #[serde(default)]
    draft: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    rationale: Option<String>,
    #[serde(default)]
    changes: Option<Vec<crate::domain::Change>>,
}

impl ReplyArgs {
    fn changes_proposal(&self) -> bool {
        self.action.is_some()
            || self.sources.is_some()
            || self.target_dir.is_some()
            || self.target_title.is_some()
            || self.draft.is_some()
            || self.tags.is_some()
            || self.rationale.is_some()
            || self.changes.is_some()
    }
}

/// Everything the model sees about a note and its neighbourhood.
struct Context {
    origin: RawNote,
    /// Readable notes the proposal may consume, by permalink.
    notes: HashMap<String, RawNote>,
    candidates: String,
    verified: String,
    verified_dirs: String,
}

impl Context {
    fn free(&self) -> HashSet<String> {
        self.notes.keys().cloned().collect()
    }
}

fn fence(n: &RawNote, max: usize) -> String {
    format!(
        "### `{}`\n\n```markdown\n{}\n```\n",
        n.permalink,
        clip(&n.raw, max)
    )
}

impl Agent {
    pub fn spawn(self) -> AgentHandle {
        let (tx, mut rx) = mpsc::channel::<Job>(QUEUE);
        let (events, _) = broadcast::channel(64);
        let state: Arc<Mutex<AgentState>> = Arc::default();
        let handle = AgentHandle {
            tx,
            events: events.clone(),
            state: Arc::clone(&state),
        };
        let agent = Arc::new(self);
        tokio::spawn(async move {
            while let Some(job) = rx.recv().await {
                let label = match &job {
                    Job::Triage { permalink } => permalink.clone(),
                    Job::Reply { id } | Job::Regenerate { id } => format!("card #{id}"),
                };
                lock(&state).current = Some(label.clone());
                let _ = events.send(AgentEvent::Started(label.clone()));
                // Each job runs in its own task so a panic fails the job, not the worker.
                let a = Arc::clone(&agent);
                let j = job.clone();
                let res = tokio::spawn(async move {
                    match &j {
                        Job::Triage { permalink } => a.triage(permalink).await.map(|_| ()),
                        Job::Reply { id } => a.reply(*id).await,
                        Job::Regenerate { id } => a.regenerate(*id).await,
                    }
                })
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("agent job panicked: {e}")));
                {
                    let mut st = lock(&state);
                    st.current = None;
                    st.queued = st.queued.saturating_sub(1);
                    if let Job::Triage { permalink } = &job {
                        st.queued_triage.remove(permalink);
                    }
                }
                let _ = match res {
                    Ok(()) => events.send(AgentEvent::Finished(label)),
                    Err(e) => {
                        tracing::warn!(job = %label, "agent job failed: {e:#}");
                        events.send(AgentEvent::Failed(format!("{label}: {e:#}")))
                    }
                };
            }
        });
        handle
    }

    /// Asks the model, retrying once with the reason when the answer is unusable.
    async fn ask<T>(
        &self,
        mut messages: Vec<ChatMessage>,
        tool: &ToolSpec,
        check: impl Fn(Value) -> Result<T, String>,
    ) -> anyhow::Result<T> {
        for attempt in 0..2 {
            let (said, problem) = match self.llm.call_tool(&messages, tool).await? {
                LlmOutcome::ToolCall(v) => match check(v.clone()) {
                    Ok(t) => return Ok(t),
                    Err(e) => (v.to_string(), e),
                },
                LlmOutcome::Text(t) => (
                    t,
                    format!("you answered with text instead of calling `{}`", tool.name),
                ),
            };
            if attempt == 1 {
                anyhow::bail!("the model gave no usable answer: {problem}");
            }
            messages.push(ChatMessage::assistant(if said.is_empty() {
                "(empty)".into()
            } else {
                said
            }));
            messages.push(ChatMessage::user(format!(
                "Your previous answer was invalid: {problem}. Call `{}` again with corrected arguments.",
                tool.name
            )));
        }
        unreachable!("the loop returns or bails on its second pass")
    }

    /// Collects neighbours of `origin`. `own` are notes the card already holds.
    async fn gather(&self, origin: RawNote, own: Vec<RawNote>) -> anyhow::Result<Context> {
        let claimed = self.db.call(|c| db::claimed_permalinks(c)).await?;
        let inbox = inbox_permalinks(self.memory.as_ref(), &self.cfg.inbox_dir).await?;
        let mut notes: HashMap<String, RawNote> = HashMap::new();
        notes.insert(origin.permalink.clone(), origin.clone());
        let mut candidates: Vec<RawNote> = own.clone();
        for n in own {
            notes.insert(n.permalink.clone(), n);
        }

        let query = format!("{} {}", origin.title, clip(body(&origin.raw), 400));
        let hits = self.memory.search(&query, 8).await?;
        let mut verified: Vec<RawNote> = Vec::new();
        let mut inbox_found = 0;
        for hit in hits {
            if notes.contains_key(&hit.permalink) {
                continue;
            }
            let folder = folder_of(&hit.permalink);
            if inbox.contains(&hit.permalink)
                && !claimed.contains(&hit.permalink)
                && inbox_found < MAX_INBOX_CANDIDATES
            {
                if let Some(n) = self.memory.read_exact(&hit.permalink).await? {
                    inbox_found += 1;
                    notes.insert(n.permalink.clone(), n.clone());
                    candidates.push(n);
                }
            } else if !inbox.contains(&hit.permalink)
                && folder == Some(self.cfg.verified_dir.as_str())
                && verified.len() < MAX_VERIFIED_CANDIDATES
                && let Some(n) = self.memory.read_exact(&hit.permalink).await?
            {
                verified.push(n);
            }
        }

        let dirs = self.memory.list_dirs(&self.cfg.verified_dir).await?;
        Ok(Context {
            origin,
            notes,
            candidates: if candidates.is_empty() {
                "(none)".into()
            } else {
                candidates
                    .iter()
                    .map(|n| fence(n, CANDIDATE_MAX))
                    .collect::<Vec<_>>()
                    .join("\n")
            },
            verified: if verified.is_empty() {
                "(none)".into()
            } else {
                verified
                    .iter()
                    .map(|n| {
                        format!(
                            "### {} (`{}`)\n\n{}\n",
                            n.title,
                            n.permalink,
                            clip(body(&n.raw), CANDIDATE_MAX)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            },
            verified_dirs: if dirs.is_empty() {
                "(none yet)".into()
            } else {
                dirs.join(", ")
            },
        })
    }

    /// Asks for a proposal about `ctx.origin` and returns it with its source rows.
    async fn propose(&self, ctx: &Context) -> anyhow::Result<(ValidatedProposal, Vec<SourceRow>)> {
        let user = render(
            &self.prompts.triage,
            &[
                ("origin_permalink", &ctx.origin.permalink),
                ("origin", &clip(&ctx.origin.raw, ORIGIN_MAX)),
                ("candidates", &ctx.candidates),
                ("verified", &ctx.verified),
                ("verified_dir", &self.cfg.verified_dir),
                ("verified_dirs", &ctx.verified_dirs),
            ],
        );
        let messages = vec![
            ChatMessage::system(self.prompts.system.clone()),
            ChatMessage::user(user),
        ];
        let free = ctx.free();
        let origin = ctx.origin.permalink.clone();
        let v = self
            .ask(messages, &proposal_tool(), |args| {
                serde_json::from_value::<AgentProposal>(args)
                    .map_err(|e| format!("arguments do not match the schema: {e}"))?
                    .validate(&ClaimContext {
                        origin: &origin,
                        free_inbox: &free,
                    })
            })
            .await?;
        let rows = v
            .sources
            .iter()
            .map(|p| ctx.notes.get(p).map(source_row))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| anyhow::anyhow!("validated source missing from context"))?;
        Ok((v, rows))
    }

    /// cancel-safe: yes — the only write is the final single-transaction insert.
    pub async fn triage(&self, permalink: &str) -> anyhow::Result<TriageResult> {
        let claimed = self.db.call(|c| db::claimed_permalinks(c)).await?;
        if claimed.contains(permalink) {
            return Ok(TriageResult::SkippedClaimed);
        }
        let Some(origin) = self.memory.read_exact(permalink).await? else {
            return Ok(TriageResult::SkippedMissing);
        };
        let ctx = self.gather(origin, Vec::new()).await?;
        let (v, sources) = self.propose(&ctx).await?;
        match self
            .db
            .call(move |c| db::insert_proposal(c, &db::NewProposal { v, sources }))
            .await
        {
            Ok(id) => Ok(TriageResult::Created(id)),
            Err(e) if e.downcast_ref::<ClaimConflict>().is_some() => {
                Ok(TriageResult::SkippedClaimed)
            }
            Err(e) => Err(e),
        }
    }

    /// Answers unsent comments on a card that is in `agent_working`.
    ///
    /// cancel-safe: NO — between `update_draft` and the final status change a
    /// cancellation leaves the card in `agent_working`; the worker never cancels
    /// jobs and `db::reset_interrupted` recovers after a restart.
    pub async fn reply(&self, id: i64) -> anyhow::Result<()> {
        let Some(card) = self.db.call(move |c| db::get_proposal(c, id)).await? else {
            anyhow::bail!("card {id} not found");
        };
        if card.status != Status::AgentWorking {
            return Ok(());
        }
        let pending = self.db.call(move |c| db::pending_human(c, id)).await?;
        if !pending.is_empty()
            && let Err(e) = self.answer(&card, &pending).await
        {
            let text = format!("The agent could not answer: {e:#}");
            self.db
                .call(move |c| {
                    db::add_message(c, id, "system", &text, true)?;
                    db::advance(
                        c,
                        id,
                        Status::AgentWorking,
                        &Event::AgentFailed {
                            back_to: Status::Ready,
                        },
                    )
                })
                .await?;
            return Ok(());
        }
        self.db
            .call(move |c| db::advance(c, id, Status::AgentWorking, &Event::AgentDone))
            .await?;
        Ok(())
    }

    async fn answer(
        &self,
        card: &db::ProposalRow,
        pending: &[db::MessageRow],
    ) -> anyhow::Result<()> {
        let id = card.id;
        let rows = self.db.call(move |c| db::sources(c, id)).await?;
        let inbox = inbox_permalinks(self.memory.as_ref(), &self.cfg.inbox_dir).await?;
        let mut live: Vec<RawNote> = Vec::new();
        for r in rows.iter().filter(|r| inbox.contains(&r.permalink)) {
            if let Some(n) = self.memory.read_exact(&r.permalink).await? {
                live.push(n);
            }
        }
        if live.is_empty() {
            anyhow::bail!("all source notes are gone from the inbox");
        }
        let free_inbox = untriaged(self.memory.as_ref(), &self.db, &self.cfg.inbox_dir).await?;
        let thread = self.db.call(move |c| db::messages(c, id)).await?;
        let pending_ids: Vec<i64> = pending.iter().map(|m| m.id).collect();

        let thread_text = thread
            .iter()
            .filter(|m| !pending_ids.contains(&m.id))
            .map(|m| {
                format!(
                    "**{} (draft v{})**: {}",
                    m.author,
                    m.draft_version,
                    format_comment(m)
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let user = render(
            &self.prompts.reply,
            &[
                ("version", &card.version.to_string()),
                ("action", card.action.as_str()),
                (
                    "sources_list",
                    &live
                        .iter()
                        .map(|n| format!("`{}`", n.permalink))
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
                (
                    "target",
                    &match (&card.target_dir, &card.target_title) {
                        (Some(d), Some(t)) => format!("{}/{d}/{t}", self.cfg.verified_dir),
                        _ => "(none)".into(),
                    },
                ),
                (
                    "tags",
                    &if card.tags.is_empty() {
                        "(none)".into()
                    } else {
                        card.tags.join(", ")
                    },
                ),
                ("rationale", &card.rationale),
                (
                    "changes",
                    &if card.changes.is_empty() {
                        "(none)".into()
                    } else {
                        card.changes
                            .iter()
                            .map(|c| serde_json::to_string(c).unwrap_or_default())
                            .collect::<Vec<_>>()
                            .join("\n")
                    },
                ),
                (
                    "draft",
                    card.draft
                        .as_deref()
                        .unwrap_or("(no draft: the notes would be deleted)"),
                ),
                (
                    "sources",
                    &live
                        .iter()
                        .map(|n| fence(n, ORIGIN_MAX))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                (
                    "candidates",
                    &if free_inbox.is_empty() {
                        "(none)".into()
                    } else {
                        free_inbox
                            .iter()
                            .take(MAX_LISTED_FREE)
                            .map(|e| format!("- `{}` — {}", e.permalink, e.title))
                            .collect::<Vec<_>>()
                            .join("\n")
                    },
                ),
                (
                    "thread",
                    &if thread_text.is_empty() {
                        "(empty)".into()
                    } else {
                        thread_text
                    },
                ),
                (
                    "pending",
                    &pending
                        .iter()
                        .map(|m| format!("- {}", format_comment(m)))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
            ],
        );
        let messages = vec![
            ChatMessage::system(self.prompts.system.clone()),
            ChatMessage::user(user),
        ];

        let mut free: HashSet<String> = free_inbox.into_iter().map(|e| e.permalink).collect();
        free.extend(live.iter().map(|n| n.permalink.clone()));
        let current_sources: Vec<String> = live.iter().map(|n| n.permalink.clone()).collect();
        let (reply, revised) = self
            .ask(messages, &reply_tool(), |args| {
                let r: ReplyArgs = serde_json::from_value(args)
                    .map_err(|e| format!("arguments do not match the schema: {e}"))?;
                if r.reply.trim().is_empty() {
                    return Err("`reply` must not be empty".into());
                }
                if !r.changes_proposal() {
                    return Ok((r.reply, None));
                }
                let sources = r.sources.clone().unwrap_or_else(|| current_sources.clone());
                // A revision may add or drop notes but must stay about this card:
                // at least one current note remains, and it anchors validation.
                let origin = sources
                    .iter()
                    .find(|s| current_sources.contains(s))
                    .cloned()
                    .ok_or_else(|| {
                        format!(
                            "`sources` must keep at least one of the card's current notes: {}",
                            current_sources.join(", ")
                        )
                    })?;
                let p = AgentProposal {
                    action: r
                        .action
                        .clone()
                        .unwrap_or_else(|| card.action.as_str().to_string()),
                    sources,
                    target_dir: r.target_dir.clone().or_else(|| card.target_dir.clone()),
                    target_title: r.target_title.clone().or_else(|| card.target_title.clone()),
                    draft: r.draft.clone().or_else(|| card.draft.clone()),
                    tags: r.tags.clone().or_else(|| Some(card.tags.clone())),
                    rationale: r
                        .rationale
                        .clone()
                        .unwrap_or_else(|| card.rationale.clone()),
                    // A new draft needs its own change notes; otherwise the
                    // notes of the current draft still apply.
                    changes: if r.draft.is_some() {
                        r.changes.clone()
                    } else {
                        Some(card.changes.clone())
                    },
                };
                let v = p.validate(&ClaimContext {
                    origin: &origin,
                    free_inbox: &free,
                })?;
                Ok((r.reply, Some(v)))
            })
            .await?;

        if let Some(v) = revised {
            let mut new_rows = Vec::new();
            for p in &v.sources {
                let note = match live.iter().find(|n| &n.permalink == p) {
                    Some(n) => n.clone(),
                    None => self
                        .memory
                        .read_exact(p)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("source {p} disappeared"))?,
                };
                new_rows.push(source_row(&note));
            }
            self.db
                .call(move |c| db::update_draft(c, id, &v, &new_rows))
                .await?;
        }
        let model = self.llm.model();
        self.db
            .call(move |c| {
                db::mark_sent_ids(c, &pending_ids)?;
                db::add_message_ext(c, id, "agent", &reply, true, None, Some(&model))
            })
            .await?;
        Ok(())
    }

    /// Rebuilds the proposal of a card in `agent_working` from its live sources.
    ///
    /// cancel-safe: NO — same reasoning as [`Agent::reply`].
    pub async fn regenerate(&self, id: i64) -> anyhow::Result<()> {
        let rows = self.db.call(move |c| db::sources(c, id)).await?;
        let inbox = inbox_permalinks(self.memory.as_ref(), &self.cfg.inbox_dir).await?;
        let mut live: Vec<RawNote> = Vec::new();
        for r in rows.iter().filter(|r| inbox.contains(&r.permalink)) {
            if let Some(n) = self.memory.read_exact(&r.permalink).await? {
                live.push(n);
            }
        }
        if live.is_empty() {
            self.db
                .call(move |c| {
                    db::release_sources(c, id)?;
                    db::add_message(
                        c,
                        id,
                        "system",
                        "All source notes are gone from the inbox; the card is closed.",
                        true,
                    )?;
                    db::advance(c, id, Status::AgentWorking, &Event::SourcesGone)
                })
                .await?;
            return Ok(());
        }
        let origin = live.remove(0);
        let outcome = async {
            let ctx = self.gather(origin, live).await?;
            let (v, sources) = self.propose(&ctx).await?;
            self.db
                .call(move |c| db::update_draft(c, id, &v, &sources))
                .await
        }
        .await;
        let (note, ev) = match outcome {
            Ok(()) => (
                "Proposal regenerated from the current source notes.".to_string(),
                Event::AgentDone,
            ),
            Err(e) => (
                format!("The agent could not regenerate the proposal: {e:#}"),
                Event::AgentFailed {
                    back_to: Status::Stale,
                },
            ),
        };
        self.db
            .call(move |c| {
                db::add_message(c, id, "system", &note, true)?;
                db::advance(c, id, Status::AgentWorking, &ev)
            })
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::db::{QueueFilter, get_proposal, list_queue, messages};
    use crate::domain::Action;
    use crate::llm::fake::FakeLlm;
    use crate::memory::fake::FakeMemory;

    struct World {
        mem: Arc<FakeMemory>,
        llm: Arc<FakeLlm>,
        db: Db,
    }

    impl World {
        fn new() -> World {
            World {
                mem: Arc::default(),
                llm: Arc::default(),
                db: Db::open_in_memory().unwrap(),
            }
        }

        fn agent(&self) -> Agent {
            Agent {
                memory: self.mem.clone(),
                llm: self.llm.clone(),
                db: self.db.clone(),
                prompts: Arc::new(Prompts::load(None).unwrap()),
                cfg: AgentCfg {
                    inbox_dir: "inbox".into(),
                    verified_dir: "verified".into(),
                },
            }
        }

        async fn card(&self, id: i64) -> db::ProposalRow {
            self.db
                .call(move |c| get_proposal(c, id))
                .await
                .unwrap()
                .unwrap()
        }

        async fn thread(&self, id: i64) -> Vec<db::MessageRow> {
            self.db.call(move |c| messages(c, id)).await.unwrap()
        }

        async fn sources(&self, id: i64) -> Vec<SourceRow> {
            self.db.call(move |c| db::sources(c, id)).await.unwrap()
        }

        async fn claimed(&self) -> HashSet<String> {
            self.db.call(|c| db::claimed_permalinks(c)).await.unwrap()
        }

        async fn set_working(&self, id: i64, from: Status) {
            assert!(
                self.db
                    .call(move |c| db::cas_status(c, id, from, Status::AgentWorking))
                    .await
                    .unwrap()
            );
        }

        async fn comment(&self, id: i64, text: &'static str) {
            self.db
                .call(move |c| db::add_message(c, id, "human", text, false))
                .await
                .unwrap();
        }
    }

    fn promote(origin: &str) -> Value {
        json!({"action": "promote", "sources": [origin], "target_dir": "infra",
               "target_title": "Clean title", "draft": "- [fact] clean", "changes": [], "rationale": "Useful."})
    }

    #[tokio::test]
    async fn triage_creates_promote() {
        let w = World::new();
        let a = w
            .mem
            .add("inbox", "Nginx reload", "- [fact] reload keeps connections");
        w.llm.push_tool(promote(&a));

        let r = w.agent().triage(&a).await.unwrap();
        let TriageResult::Created(id) = r else {
            panic!("{r:?}")
        };
        let card = w.card(id).await;
        assert_eq!(card.status, Status::Ready);
        assert_eq!(card.action, Action::Promote);
        assert_eq!(card.target_dir.as_deref(), Some("infra"));
        let src = w.sources(id).await;
        assert_eq!(src[0].content_hash, content_hash(&w.mem.raw(&a).unwrap()));
        assert_eq!(src[0].title, "Nginx reload");

        // The prompt carried the note text and asked for the tool.
        let seen = w.llm.seen.lock().unwrap();
        assert!(seen[0][1].content.contains("reload keeps connections"));
        assert_eq!(seen[0][0].role, "system");
    }

    #[tokio::test]
    async fn triage_lists_verified_folders_and_notes() {
        let w = World::new();
        let a = w.mem.add("inbox", "A", "- [fact] a");
        w.mem.add("verified/infra", "Known", "- [fact] known thing");
        w.llm.push_tool(promote(&a));
        w.agent().triage(&a).await.unwrap();
        let seen = w.llm.seen.lock().unwrap();
        let user = &seen[0][1].content;
        assert!(user.contains("infra"), "{user}");
        assert!(user.contains("known thing"), "{user}");
    }

    #[tokio::test]
    async fn triage_merge_claims_neighbours_and_skips_them_later() {
        let w = World::new();
        let a = w.mem.add("inbox", "A", "- [fact] same topic 1");
        let b = w.mem.add("inbox", "B", "- [fact] same topic 2");
        w.llm.push_tool(
            json!({"action": "merge", "sources": [a, b], "target_dir": "infra",
            "target_title": "Topic", "draft": "- [fact] merged", "changes": [], "rationale": "Same topic."}),
        );

        let TriageResult::Created(id) = w.agent().triage(&a).await.unwrap() else {
            panic!()
        };
        assert_eq!(w.card(id).await.action, Action::Merge);
        assert_eq!(w.sources(id).await.len(), 2);
        // The candidate's text was offered to the model.
        assert!(
            w.llm.seen.lock().unwrap()[0][1]
                .content
                .contains("same topic 2")
        );

        assert_eq!(
            w.agent().triage(&b).await.unwrap(),
            TriageResult::SkippedClaimed
        );
    }

    #[tokio::test]
    async fn merge_does_not_steal_claimed_note() {
        let w = World::new();
        let a = w.mem.add("inbox", "A", "- [fact] a");
        let b = w.mem.add("inbox", "B", "- [fact] b");
        w.llm.push_tool(promote(&b));
        w.agent().triage(&b).await.unwrap();

        let steal = json!({"action": "merge", "sources": [a, b], "target_dir": "infra",
            "target_title": "T", "draft": "d", "changes": [], "rationale": "r"});
        w.llm.push_tool(steal.clone());
        w.llm.push_tool(steal);
        assert!(w.agent().triage(&a).await.is_err());
        let all =
            w.db.call(|c| list_queue(c, QueueFilter::All))
                .await
                .unwrap();
        assert_eq!(all.len(), 1);
        // The retry told the model what was wrong.
        let seen = w.llm.seen.lock().unwrap();
        assert!(seen[2].last().unwrap().content.contains(&b));
    }

    #[tokio::test]
    async fn note_moved_out_of_inbox_is_not_a_candidate() {
        let w = World::new();
        let a = w.mem.add("inbox", "A", "- [fact] a");
        let b = w
            .mem
            .add("inbox", "B", "- [fact] b, already verified by hand");
        w.mem.move_to(&b, "verified/ops");
        let grab = json!({"action": "merge", "sources": [a, b], "target_dir": "infra",
            "target_title": "T", "draft": "d", "changes": [], "rationale": "r"});
        w.llm.push_tool(grab.clone());
        w.llm.push_tool(grab);
        assert!(w.agent().triage(&a).await.is_err());
        assert!(w.claimed().await.is_empty());
    }

    #[tokio::test]
    async fn triage_skips_missing_note() {
        let w = World::new();
        assert_eq!(
            w.agent().triage("p/inbox/gone").await.unwrap(),
            TriageResult::SkippedMissing
        );
        assert!(w.llm.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn text_then_tool_call_succeeds_on_retry() {
        let w = World::new();
        let a = w.mem.add("inbox", "A", "- [fact] a");
        w.llm.push_text("Sure, I think it should be promoted.");
        w.llm.push_tool(promote(&a));
        assert!(matches!(
            w.agent().triage(&a).await.unwrap(),
            TriageResult::Created(_)
        ));
        let seen = w.llm.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        let retry = &seen[1];
        assert_eq!(retry[retry.len() - 2].role, "assistant");
        assert!(retry.last().unwrap().content.contains("submit_proposal"));
    }

    #[tokio::test]
    async fn invalid_reply_retries_then_system_message() {
        let w = World::new();
        let a = w.mem.add("inbox", "A", "- [fact] a");
        w.llm.push_tool(promote(&a));
        let TriageResult::Created(id) = w.agent().triage(&a).await.unwrap() else {
            panic!()
        };
        w.comment(id, "Shorter please").await;
        w.set_working(id, Status::Ready).await;
        w.llm.push_text("nope");
        w.llm.push_text("nope again");

        w.agent().reply(id).await.unwrap();
        let card = w.card(id).await;
        assert_eq!(card.status, Status::Ready);
        assert_eq!(card.version, 1);
        let thread = w.thread(id).await;
        assert_eq!(thread.last().unwrap().author, "system");
        // The comment stays unsent so the human can send it again.
        let pending = w.db.call(move |c| db::pending_human(c, id)).await.unwrap();
        assert_eq!(pending.len(), 1);
    }

    #[tokio::test]
    async fn reply_with_new_draft_bumps_version() {
        let w = World::new();
        let a = w.mem.add("inbox", "A", "- [fact] a");
        w.llm.push_tool(promote(&a));
        let TriageResult::Created(id) = w.agent().triage(&a).await.unwrap() else {
            panic!()
        };
        w.comment(id, "Put it under ops").await;
        w.set_working(id, Status::Ready).await;
        w.llm.push_tool(
            json!({"reply": "Moved to ops.", "target_dir": "ops", "draft": "- [fact] v2", "changes": []}),
        );

        w.agent().reply(id).await.unwrap();
        let card = w.card(id).await;
        assert_eq!(card.status, Status::Ready);
        assert_eq!(card.version, 2);
        assert_eq!(card.target_dir.as_deref(), Some("ops"));
        assert_eq!(card.draft.as_deref(), Some("- [fact] v2"));
        assert_eq!(card.target_title.as_deref(), Some("Clean title"));
        let thread = w.thread(id).await;
        assert_eq!(thread.last().unwrap().author, "agent");
        assert_eq!(thread.last().unwrap().body, "Moved to ops.");
        assert_eq!(thread.last().unwrap().draft_version, 2);
        assert!(thread.iter().all(|m| m.sent));
        // The comment reached the model.
        assert!(
            w.llm.seen.lock().unwrap()[1][1]
                .content
                .contains("Put it under ops")
        );
    }

    #[tokio::test]
    async fn reply_can_update_rationale() {
        let w = World::new();
        let a = w.mem.add("inbox", "A", "- [fact] a");
        w.llm.push_tool(promote(&a));
        let TriageResult::Created(id) = w.agent().triage(&a).await.unwrap() else {
            panic!()
        };
        w.comment(id, "Drop the numbers").await;
        w.set_working(id, Status::Ready).await;
        w.llm
            .push_tool(json!({"reply": "Done.", "draft": "- [fact] no numbers",
                               "rationale": "Numbers removed at the reviewer's request.", "changes": []}));
        w.agent().reply(id).await.unwrap();
        assert_eq!(
            w.card(id).await.rationale,
            "Numbers removed at the reviewer's request."
        );
    }

    #[tokio::test]
    async fn reply_without_draft_keeps_version() {
        let w = World::new();
        let a = w.mem.add("inbox", "A", "- [fact] a");
        w.llm.push_tool(promote(&a));
        let TriageResult::Created(id) = w.agent().triage(&a).await.unwrap() else {
            panic!()
        };
        w.comment(id, "Why infra?").await;
        w.set_working(id, Status::Ready).await;
        w.llm
            .push_tool(json!({"reply": "Because it is about servers."}));
        w.agent().reply(id).await.unwrap();
        assert_eq!(w.card(id).await.version, 1);
    }

    #[tokio::test]
    async fn reply_cannot_swap_out_all_sources() {
        let w = World::new();
        let a = w.mem.add("inbox", "A", "- [fact] a");
        let b = w.mem.add("inbox", "B", "- [fact] unrelated");
        w.llm.push_tool(promote(&a));
        let TriageResult::Created(id) = w.agent().triage(&a).await.unwrap() else {
            panic!()
        };
        w.comment(id, "ok?").await;
        w.set_working(id, Status::Ready).await;
        let swap = json!({"reply": "Switched.", "action": "delete", "sources": [b]});
        w.llm.push_tool(swap.clone());
        w.llm.push_tool(swap);
        w.agent().reply(id).await.unwrap();
        let src = w.sources(id).await;
        assert_eq!(
            src.iter().map(|s| s.permalink.clone()).collect::<Vec<_>>(),
            std::slice::from_ref(&a)
        );
        assert_eq!(w.card(id).await.action, Action::Promote);
        assert!(!w.claimed().await.contains(&b));
        assert_eq!(w.thread(id).await.last().unwrap().author, "system");
    }

    #[tokio::test]
    async fn reply_change_to_delete_clears_draft() {
        let w = World::new();
        let a = w.mem.add("inbox", "A", "- [fact] a");
        w.llm.push_tool(promote(&a));
        let TriageResult::Created(id) = w.agent().triage(&a).await.unwrap() else {
            panic!()
        };
        w.comment(id, "This is junk, drop it").await;
        w.set_working(id, Status::Ready).await;
        w.llm
            .push_tool(json!({"reply": "Agreed.", "action": "delete"}));
        w.agent().reply(id).await.unwrap();
        let card = w.card(id).await;
        assert_eq!(card.action, Action::Delete);
        assert!(card.draft.is_none());
    }

    #[tokio::test]
    async fn regenerate_drops_missing_sources_or_closes() {
        let w = World::new();
        let a = w.mem.add("inbox", "A", "- [fact] a");
        let b = w.mem.add("inbox", "B", "- [fact] b");
        w.llm.push_tool(
            json!({"action": "merge", "sources": [a, b], "target_dir": "infra",
            "target_title": "T", "draft": "d", "changes": [], "rationale": "r"}),
        );
        let TriageResult::Created(id) = w.agent().triage(&a).await.unwrap() else {
            panic!()
        };

        w.mem.remove(&b);
        w.db.call(move |c| db::cas_status(c, id, Status::Ready, Status::Stale))
            .await
            .unwrap();
        w.set_working(id, Status::Stale).await;
        w.llm.push_tool(promote(&a));
        w.agent().regenerate(id).await.unwrap();
        let card = w.card(id).await;
        assert_eq!(card.status, Status::Ready);
        assert_eq!(card.version, 2);
        assert_eq!(
            w.sources(id)
                .await
                .iter()
                .map(|s| s.permalink.clone())
                .collect::<Vec<_>>(),
            std::slice::from_ref(&a)
        );

        w.mem.remove(&a);
        w.db.call(move |c| db::cas_status(c, id, Status::Ready, Status::Stale))
            .await
            .unwrap();
        w.set_working(id, Status::Stale).await;
        w.agent().regenerate(id).await.unwrap();
        assert_eq!(w.card(id).await.status, Status::Closed);
        assert!(w.claimed().await.is_empty());
    }

    #[tokio::test]
    async fn regenerate_failure_returns_to_stale() {
        let w = World::new();
        let a = w.mem.add("inbox", "A", "- [fact] a");
        w.llm.push_tool(promote(&a));
        let TriageResult::Created(id) = w.agent().triage(&a).await.unwrap() else {
            panic!()
        };
        w.db.call(move |c| db::cas_status(c, id, Status::Ready, Status::Stale))
            .await
            .unwrap();
        w.set_working(id, Status::Stale).await;
        w.llm.push_text("x");
        w.llm.push_text("y");
        w.agent().regenerate(id).await.unwrap();
        assert_eq!(w.card(id).await.status, Status::Stale);
        assert_eq!(w.thread(id).await.last().unwrap().author, "system");
    }

    #[tokio::test]
    async fn untriaged_excludes_claimed() {
        let w = World::new();
        let a = w.mem.add("inbox", "A", "- [fact] a");
        let b = w.mem.add("inbox", "B", "- [fact] b");
        w.mem.add("verified", "V", "- [fact] v");
        w.llm.push_tool(promote(&a));
        w.agent().triage(&a).await.unwrap();
        let left = untriaged(w.mem.as_ref(), &w.db, "inbox").await.unwrap();
        assert_eq!(
            left.iter().map(|e| e.permalink.clone()).collect::<Vec<_>>(),
            [b]
        );
    }

    #[tokio::test]
    async fn worker_runs_jobs_sequentially_and_emits_events() {
        let w = World::new();
        let a = w.mem.add("inbox", "A", "- [fact] a");
        let b = w.mem.add("inbox", "B", "- [fact] b");
        w.llm.push_tool(promote(&a));
        w.llm.push_tool(promote(&b));
        let h = w.agent().spawn();
        let mut rx = h.events.subscribe();
        assert!(h.enqueue(Job::Triage {
            permalink: a.clone()
        }));
        assert!(
            !h.enqueue(Job::Triage {
                permalink: a.clone()
            }),
            "duplicate triage refused"
        );
        assert!(h.enqueue(Job::Triage {
            permalink: b.clone()
        }));

        let mut finished = 0;
        while finished < 2 {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                AgentEvent::Finished(_) => finished += 1,
                AgentEvent::Failed(e) => panic!("{e}"),
                _ => {}
            }
        }
        {
            let st = h.state.lock().unwrap();
            assert!(st.queued_triage.is_empty());
            assert_eq!(st.queued, 0);
            assert!(st.current.is_none());
        }
        assert_eq!(w.claimed().await.len(), 2);
    }

    #[tokio::test]
    async fn worker_reports_failures() {
        let w = World::new();
        let a = w.mem.add("inbox", "A", "- [fact] a");
        w.llm.push_text("x");
        w.llm.push_text("y");
        let h = w.agent().spawn();
        let mut rx = h.events.subscribe();
        h.enqueue(Job::Triage { permalink: a });
        loop {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                AgentEvent::Failed(_) => break,
                AgentEvent::Finished(m) => panic!("unexpected success {m}"),
                _ => {}
            }
        }
    }

    fn ch(kind: &str, text: &str) -> Value {
        json!({"kind": kind, "text": text, "source": "p/inbox/a", "why": "because"})
    }

    #[tokio::test]
    async fn triage_stores_changes() {
        let w = World::new();
        let a = w.mem.add("inbox", "A", "- [fact] a");
        let mut p = promote(&a);
        p["changes"] = json!([ch("added", "- [fact] clean"), ch("removed", "- [fact] a")]);
        w.llm.push_tool(p);
        let TriageResult::Created(id) = w.agent().triage(&a).await.unwrap() else {
            panic!()
        };
        let card = w.card(id).await;
        assert_eq!(card.changes.len(), 2);
        assert_eq!(card.changes[1].source.as_deref(), Some("p/inbox/a"));
    }

    #[tokio::test]
    async fn missing_changes_retries() {
        let w = World::new();
        let a = w.mem.add("inbox", "A", "- [fact] a");
        let mut bad = promote(&a);
        bad.as_object_mut().unwrap().remove("changes");
        w.llm.push_tool(bad);
        w.llm.push_tool(promote(&a));
        assert!(matches!(
            w.agent().triage(&a).await.unwrap(),
            TriageResult::Created(_)
        ));
        let seen = w.llm.seen.lock().unwrap();
        assert!(seen[1].last().unwrap().content.contains("changes"));
    }

    #[tokio::test]
    async fn reply_new_draft_requires_changes_and_stores_them() {
        let w = World::new();
        let a = w.mem.add("inbox", "A", "- [fact] a");
        w.llm.push_tool(promote(&a));
        let TriageResult::Created(id) = w.agent().triage(&a).await.unwrap() else {
            panic!()
        };
        w.comment(id, "Add the date").await;
        w.set_working(id, Status::Ready).await;
        w.llm
            .push_tool(json!({"reply": "Added.", "draft": "- [fact] clean on 05.10"}));
        w.llm.push_tool(
            json!({"reply": "Added.", "draft": "- [fact] clean on 05.10",
                               "changes": [ch("rewritten", "clean on 05.10")]}),
        );
        w.agent().reply(id).await.unwrap();
        let card = w.card(id).await;
        assert_eq!(card.version, 2);
        assert_eq!(card.changes[0].text, "clean on 05.10");
    }

    #[tokio::test]
    async fn reply_without_draft_keeps_changes() {
        let w = World::new();
        let a = w.mem.add("inbox", "A", "- [fact] a");
        let mut p = promote(&a);
        p["changes"] = json!([ch("added", "- [fact] clean")]);
        w.llm.push_tool(p);
        let TriageResult::Created(id) = w.agent().triage(&a).await.unwrap() else {
            panic!()
        };
        w.comment(id, "Other folder").await;
        w.set_working(id, Status::Ready).await;
        w.llm
            .push_tool(json!({"reply": "Moved.", "target_dir": "ops"}));
        w.agent().reply(id).await.unwrap();
        let card = w.card(id).await;
        assert_eq!(card.target_dir.as_deref(), Some("ops"));
        assert_eq!(card.changes.len(), 1);
    }

    #[tokio::test]
    async fn agent_message_records_model() {
        let w = World::new();
        let a = w.mem.add("inbox", "A", "- [fact] a");
        w.llm.push_tool(promote(&a));
        let TriageResult::Created(id) = w.agent().triage(&a).await.unwrap() else {
            panic!()
        };
        w.comment(id, "?").await;
        w.set_working(id, Status::Ready).await;
        w.llm.push_tool(json!({"reply": "Answer."}));
        w.agent().reply(id).await.unwrap();
        let last = w.thread(id).await.pop().unwrap();
        assert_eq!(last.model.as_deref(), Some("fake-model"));
    }

    #[tokio::test]
    async fn anchored_comment_reaches_prompt() {
        let w = World::new();
        let a = w.mem.add("inbox", "A", "- [fact] a");
        w.llm.push_tool(promote(&a));
        let TriageResult::Created(id) = w.agent().triage(&a).await.unwrap() else {
            panic!()
        };
        w.db.call(move |c| {
            db::add_message_ext(
                c,
                id,
                "human",
                "wrong",
                false,
                Some(&Anchor::Draft {
                    version: 1,
                    quote: "QUOTED BIT".into(),
                }),
                None,
            )
        })
        .await
        .unwrap();
        w.set_working(id, Status::Ready).await;
        w.llm.push_tool(json!({"reply": "Fixed."}));
        w.agent().reply(id).await.unwrap();
        let seen = w.llm.seen.lock().unwrap();
        assert!(
            seen[1][1].content.contains("“QUOTED BIT”"),
            "{}",
            seen[1][1].content
        );
    }

    #[test]
    fn format_comment_variants() {
        let mut m = db::MessageRow {
            id: 1,
            author: "human".into(),
            body: "text".into(),
            draft_version: 2,
            sent: false,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            anchor: None,
            model: None,
        };
        assert_eq!(format_comment(&m), "text");
        m.anchor = Some(Anchor::Draft {
            version: 2,
            quote: "q".into(),
        });
        assert_eq!(format_comment(&m), "Comment on draft v2 fragment “q”: text");
        m.anchor = Some(Anchor::Diff {
            permalink: "p/inbox/a".into(),
            line: "- x".into(),
        });
        assert_eq!(
            format_comment(&m),
            "Comment on diff line of `p/inbox/a`: “- x”: text"
        );
    }

    #[test]
    fn reply_prompt_has_changes_slot() {
        assert!(Prompts::load(None).unwrap().reply.contains("{{changes}}"));
    }
}
