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
use crate::domain::{Event, Status, can_comment};

use super::AppState;
use super::views::{
    CardFragment, HeaderFragment, IndexPage, QueueFragment, header_view, load_card, queue_view,
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
    if let Err(e) = apply::refresh(&s.db, s.memory.as_ref(), &open).await {
        tracing::warn!("checking sources for changes failed: {e:#}");
    }
    let queue = queue_view(s, filter).await?;
    let id = id.or_else(|| queue.rows.first().map(|r| r.id));
    let card = match id {
        Some(id) => load_card(s, id, None).await?,
        None => None,
    };
    html(IndexPage {
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
    if let Err(e) = apply::refresh(&s.db, s.memory.as_ref(), &[id]).await {
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

/// Starts an agent job for a card already moved to `agent_working`; undoes
/// the move if the queue refuses the job.
async fn start_job(s: &AppState, id: i64, job: Job, back_to: Status) -> Result<Option<String>> {
    if s.agent.enqueue(job) {
        return Ok(None);
    }
    s.db.call(move |c| db::advance(c, id, Status::AgentWorking, &Event::AgentFailed { back_to }))
        .await?;
    Ok(Some(s.t.queue_full.to_string()))
}

#[derive(Deserialize)]
pub struct CommentForm {
    #[serde(default)]
    body: String,
    #[serde(default)]
    send: String,
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
    if !text.is_empty() && can_comment(status) {
        s.db.call(move |c| db::add_message(c, id, "human", &text, false))
            .await?;
    }
    let mut notice = None;
    if f.send == "1" {
        let pending = s.db.call(move |c| db::pending_human(c, id)).await?;
        if !pending.is_empty()
            && s.db
                .call(move |c| db::advance(c, id, Status::Ready, &Event::SendToAgent))
                .await?
        {
            notice = start_job(&s, id, Job::Reply { id }, Status::Ready).await?;
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

pub async fn accept(State(s): State<AppState>, Path(id): Path<i64>) -> Result<Response> {
    if status_of(&s, id).await?.is_none() {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let r = apply::accept(&s.db, s.memory.as_ref(), &s.cfg.verified_dir, id).await?;
    card_response(&s, load_card(&s, id, apply_notice(&s, &r)).await?)
}

pub async fn retry(State(s): State<AppState>, Path(id): Path<i64>) -> Result<Response> {
    if status_of(&s, id).await?.is_none() {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let r = apply::retry(&s.db, s.memory.as_ref(), &s.cfg.verified_dir, id).await?;
    card_response(&s, load_card(&s, id, apply_notice(&s, &r)).await?)
}

pub async fn snooze(State(s): State<AppState>, Path(id): Path<i64>) -> Result<Response> {
    let until = OffsetDateTime::now_utc() + time::Duration::days(s.cfg.snooze_days);
    s.db.call(move |c| db::snooze(c, id, until)).await?;
    card_response(&s, load_card(&s, id, None).await?)
}

pub async fn regenerate(State(s): State<AppState>, Path(id): Path<i64>) -> Result<Response> {
    let mut notice = None;
    if s.db
        .call(move |c| db::advance(c, id, Status::Stale, &Event::Regenerate))
        .await?
    {
        notice = start_job(&s, id, Job::Regenerate { id }, Status::Stale).await?;
    }
    card_response(&s, load_card(&s, id, notice).await?)
}
