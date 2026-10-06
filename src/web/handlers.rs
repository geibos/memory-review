//! Request handlers.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use time::OffsetDateTime;

use crate::agent::{Job, untriaged};
use crate::apply::{self, ApplyResult};
use crate::db::{self, QueueFilter};
use crate::domain::{Anchor, Event, Status, can_comment};

use super::AppState;
use super::views::{
    CardFragment, HeaderFragment, IndexPage, QueueFragment, SettingsPage, header_view, load_card,
    queue_view,
};

const MAX_COMMENT: usize = 20_000;

/// Any internal failure: logged, shown as a short 500.
pub struct AppError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError(e.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::error!("request failed: {:#}", self.0);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal error, see the service log",
        )
            .into_response()
    }
}

type Result<T> = std::result::Result<T, AppError>;

fn html(t: impl Template) -> Result<Response> {
    Ok(Html(t.render()?).into_response())
}

/// A card fragment that also asks the page to refresh the header and queue.
fn card_response(s: &AppState, card: Option<super::views::CardView>) -> Result<Response> {
    let Some(card) = card else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let body = CardFragment {
        t: s.t,
        card: Some(card),
    }
    .render()?;
    Ok(([("HX-Trigger", "mr:queue")], Html(body)).into_response())
}

#[derive(Deserialize)]
pub struct IndexQuery {
    id: Option<i64>,
    filter: Option<String>,
}

pub async fn index(State(s): State<AppState>, Query(q): Query<IndexQuery>) -> Result<Response> {
    page(&s, q.id, q.filter.as_deref()).await
}

async fn page(s: &AppState, id: Option<i64>, filter: Option<&str>) -> Result<Response> {
    let now = OffsetDateTime::now_utc();
    s.db.call(move |c| db::wake_snoozed(c, now)).await?;
    let open: Vec<i64> =
        s.db.call(|c| db::list_queue(c, QueueFilter::Open))
            .await?
            .iter()
            .map(|i| i.id)
            .collect();
    if let Err(e) = apply::refresh(&s.db, s.memory.as_ref(), &s.cfg.inbox_dir, &open).await {
        tracing::warn!("checking sources for changes failed: {e:#}");
    }
    let queue = queue_view(s, filter).await?;
    let id = id.or_else(|| queue.rows.first().map(|r| r.id));
    let card = match id {
        Some(id) => load_card(s, id, None).await?,
        None => None,
    };
    html(IndexPage {
        v: super::asset_version(),
        t: s.t,
        header: header_view(s).await?,
        queue,
        card,
    })
}

pub async fn header(State(s): State<AppState>) -> Result<Response> {
    html(HeaderFragment {
        t: s.t,
        header: header_view(&s).await?,
    })
}

#[derive(Deserialize)]
pub struct QueueQuery {
    filter: Option<String>,
}

pub async fn queue(State(s): State<AppState>, Query(q): Query<QueueQuery>) -> Result<Response> {
    let now = OffsetDateTime::now_utc();
    s.db.call(move |c| db::wake_snoozed(c, now)).await?;
    html(QueueFragment {
        t: s.t,
        queue: queue_view(&s, q.filter.as_deref()).await?,
    })
}

pub async fn card(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<Response> {
    if let Err(e) = apply::refresh(&s.db, s.memory.as_ref(), &s.cfg.inbox_dir, &[id]).await {
        tracing::warn!("checking sources of card {id} failed: {e:#}");
    }
    if !headers.contains_key("hx-request") {
        return page(&s, Some(id), None).await;
    }
    match load_card(&s, id, None).await? {
        Some(card) => html(CardFragment {
            t: s.t,
            card: Some(card),
        }),
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

pub async fn triage(State(s): State<AppState>) -> Result<Response> {
    for entry in untriaged(s.memory.as_ref(), &s.db, &s.cfg.inbox_dir).await? {
        s.agent.enqueue(Job::Triage {
            permalink: entry.permalink,
        });
    }
    html(HeaderFragment {
        t: s.t,
        header: header_view(&s).await?,
    })
}

async fn status_of(s: &AppState, id: i64) -> Result<Option<Status>> {
    Ok(s.db
        .call(move |c| db::get_proposal(c, id))
        .await?
        .map(|p| p.status))
}

/// Moves a card to `agent_working` and queues its job, as one unit.
///
/// cancel-safe: yes — the work runs in its own task, so a client that goes
/// away between the status change and the enqueue cannot strand the card in
/// `agent_working` with no job. Undoes the move if the queue refuses the job.
async fn hand_to_agent(
    s: &AppState,
    id: i64,
    from: Status,
    ev: Event,
    job: Job,
) -> Result<Option<String>> {
    let s = s.clone();
    let task = tokio::spawn(async move {
        if !s.db.call(move |c| db::advance(c, id, from, &ev)).await? {
            return anyhow::Ok(None);
        }
        if s.agent.enqueue(job) {
            return Ok(None);
        }
        s.db.call(move |c| {
            db::advance(
                c,
                id,
                Status::AgentWorking,
                &Event::AgentFailed { back_to: from },
            )
        })
        .await?;
        Ok(Some(s.t.queue_full.to_string()))
    });
    Ok(task.await??)
}

#[derive(Deserialize)]
pub struct CommentForm {
    #[serde(default)]
    body: String,
    #[serde(default)]
    send: String,
    /// `draft`, `diff` or empty for a comment on the whole card.
    #[serde(default)]
    anchor: String,
    #[serde(default)]
    quote: String,
    #[serde(default)]
    permalink: String,
    #[serde(default)]
    line: String,
}

const MAX_QUOTE: usize = 500;

fn clip_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

pub async fn comment(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<CommentForm>,
) -> Result<Response> {
    let Some(status) = status_of(&s, id).await? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let text = f.body.trim().to_string();
    if text.chars().count() > MAX_COMMENT {
        return Ok((StatusCode::BAD_REQUEST, "comment is too long").into_response());
    }
    let anchor = match f.anchor.as_str() {
        "" => None,
        "draft" => {
            let quote = clip_chars(f.quote.trim(), MAX_QUOTE);
            if quote.is_empty() {
                return Ok((StatusCode::BAD_REQUEST, "quote is empty").into_response());
            }
            let version =
                s.db.call(move |c| db::get_proposal(c, id))
                    .await?
                    .map(|p| p.version)
                    .unwrap_or(1);
            Some(Anchor::Draft { version, quote })
        }
        "diff" => {
            let line = clip_chars(&f.line, MAX_QUOTE);
            if f.permalink.trim().is_empty() || line.trim().is_empty() {
                return Ok((StatusCode::BAD_REQUEST, "diff line is empty").into_response());
            }
            Some(Anchor::Diff {
                permalink: f.permalink.trim().to_string(),
                line,
            })
        }
        _ => return Ok((StatusCode::BAD_REQUEST, "unknown anchor").into_response()),
    };
    if !text.is_empty() && can_comment(status) {
        s.db.call(move |c| {
            db::add_message_ext(c, id, "human", &text, false, anchor.as_ref(), None)
        })
        .await?;
    }
    let mut notice = None;
    if f.send == "1" {
        let pending = s.db.call(move |c| db::pending_human(c, id)).await?;
        if !pending.is_empty() {
            notice =
                hand_to_agent(&s, id, Status::Ready, Event::SendToAgent, Job::Reply { id }).await?;
        }
    }
    card_response(&s, load_card(&s, id, notice).await?)
}

fn apply_notice(s: &AppState, r: &ApplyResult) -> Option<String> {
    match r {
        ApplyResult::Accepted | ApplyResult::Failed(_) => None,
        ApplyResult::Stale => Some(s.t.result_stale.to_string()),
        ApplyResult::AlreadyHandled => Some(s.t.result_already.to_string()),
    }
}

#[derive(Deserialize)]
pub struct AcceptForm {
    version: Option<i64>,
}

pub async fn accept(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Form(f): Form<AcceptForm>,
) -> Result<Response> {
    let Some(card) = s.db.call(move |c| db::get_proposal(c, id)).await? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    // The draft can only change outside `ready`, and accept itself requires
    // `ready`, so checking the version here is enough.
    if f.version != Some(card.version) {
        let notice = Some(s.t.draft_changed.to_string());
        return card_response(&s, load_card(&s, id, notice).await?);
    }
    let r = apply::accept(
        &s.db,
        s.memory.as_ref(),
        &s.cfg.inbox_dir,
        &s.cfg.verified_dir,
        id,
    )
    .await?;
    card_response(&s, load_card(&s, id, apply_notice(&s, &r)).await?)
}

pub async fn retry(State(s): State<AppState>, Path(id): Path<i64>) -> Result<Response> {
    if status_of(&s, id).await?.is_none() {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let r = apply::retry(
        &s.db,
        s.memory.as_ref(),
        &s.cfg.inbox_dir,
        &s.cfg.verified_dir,
        id,
    )
    .await?;
    card_response(&s, load_card(&s, id, apply_notice(&s, &r)).await?)
}

pub async fn snooze(State(s): State<AppState>, Path(id): Path<i64>) -> Result<Response> {
    let until = OffsetDateTime::now_utc() + time::Duration::days(s.cfg.snooze_days);
    s.db.call(move |c| db::snooze(c, id, until)).await?;
    card_response(&s, load_card(&s, id, None).await?)
}

pub async fn regenerate(State(s): State<AppState>, Path(id): Path<i64>) -> Result<Response> {
    let notice = hand_to_agent(
        &s,
        id,
        Status::Stale,
        Event::Regenerate,
        Job::Regenerate {
            id,
            model: None,
            back_to: Status::Stale,
        },
    )
    .await?;
    card_response(&s, load_card(&s, id, notice).await?)
}

#[derive(Deserialize)]
pub struct SettingsQuery {
    saved: Option<String>,
}

async fn settings_page(s: &AppState, saved: bool, error: Option<String>) -> Result<String> {
    let current = crate::llm::current_model(&s.model);
    let (mut models, catalog_ok) = match s.catalog.list_models().await {
        Ok(m) => (m, true),
        Err(e) => {
            tracing::warn!("listing models failed: {e:#}");
            (Vec::new(), false)
        }
    };
    if !models.contains(&current) {
        models.insert(0, current.clone());
    }
    Ok(SettingsPage {
        v: super::asset_version(),
        t: s.t,
        header: header_view(s).await?,
        models,
        current,
        endpoint: s.cfg.llm_url.clone(),
        catalog_ok,
        saved,
        error,
    }
    .render()?)
}

pub async fn settings(
    State(s): State<AppState>,
    Query(q): Query<SettingsQuery>,
) -> Result<Response> {
    Ok(Html(settings_page(&s, q.saved.is_some(), None).await?).into_response())
}

#[derive(Deserialize)]
pub struct SettingsForm {
    #[serde(default)]
    model: String,
    #[serde(default)]
    model_manual: String,
}

fn valid_model(name: &str) -> bool {
    !name.is_empty() && name.chars().count() <= 200 && !name.chars().any(char::is_whitespace)
}

pub async fn save_settings(
    State(s): State<AppState>,
    Form(f): Form<SettingsForm>,
) -> Result<Response> {
    let manual = f.model_manual.trim();
    let name = if manual.is_empty() {
        f.model.as_str()
    } else {
        manual
    }
    .to_string();
    if !valid_model(&name) {
        let page = settings_page(&s, false, Some(s.t.bad_model.to_string())).await?;
        return Ok((StatusCode::BAD_REQUEST, Html(page)).into_response());
    }
    let stored = name.clone();
    s.db.call(move |c| db::set_setting(c, "model", &stored))
        .await?;
    crate::llm::set_model(&s.model, &name);
    tracing::info!(model = %name, "model changed from the settings page");
    Ok((StatusCode::SEE_OTHER, [("location", "/settings?saved=1")]).into_response())
}
