//! HTTP layer: routes, same-origin guard, live events and static assets.

mod handlers;
mod views;

#[cfg(test)]
mod tests;

use std::convert::Infallible;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::Stream;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::agent::AgentHandle;
use crate::config::Config;
use crate::db::Db;
use crate::i18n::Strings;
use crate::memory::MemoryApi;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub db: Db,
    pub memory: Arc<dyn MemoryApi>,
    pub agent: AgentHandle,
    pub t: &'static Strings,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(handlers::index))
        .route("/header", get(handlers::header))
        .route("/queue", get(handlers::queue))
        .route("/p/{id}", get(handlers::card))
        .route("/triage", post(handlers::triage))
        .route("/p/{id}/comment", post(handlers::comment))
        .route("/p/{id}/accept", post(handlers::accept))
        .route("/p/{id}/retry", post(handlers::retry))
        .route("/p/{id}/snooze", post(handlers::snooze))
        .route("/p/{id}/regenerate", post(handlers::regenerate))
        .route("/events", get(events))
        .route("/healthz", get(|| async { "ok" }))
        .route("/static/{*path}", get(static_file))
        .layer(middleware::from_fn_with_state(state.clone(), same_origin))
        .with_state(state)
}

/// State-changing requests must come from our own pages. Authentication is
/// done by the reverse proxy with a cookie, so without this check any site
/// could make the browser press "Accept".
async fn same_origin(State(s): State<AppState>, req: Request, next: Next) -> Response {
    if matches!(*req.method(), Method::GET | Method::HEAD) {
        return next.run(req).await;
    }
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok());
    if origin == Some(s.cfg.public_origin.as_str()) {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, "cross-origin request refused").into_response()
    }
}

/// Server-sent events: one `changed` event per agent event.
async fn events(
    State(s): State<AppState>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let stream = BroadcastStream::new(s.agent.events.subscribe()).map(|ev| {
        // A lagging client only needs to know that something changed.
        let data = match ev {
            Ok(e) => format!("{e:?}"),
            Err(_) => "lagged".to_string(),
        };
        Ok(SseEvent::default().event("changed").data(data))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

macro_rules! assets {
    ($($name:literal => $mime:literal),* $(,)?) => {
        fn asset(path: &str) -> Option<(&'static [u8], &'static str)> {
            match path {
                $($name => Some((include_bytes!(concat!("../../static/", $name)), $mime)),)*
                _ => None,
            }
        }
    };
}

assets! {
    "app.css" => "text/css; charset=utf-8",
    "app.js" => "text/javascript; charset=utf-8",
    "htmx.min.js" => "text/javascript; charset=utf-8",
    "fonts/inter-latin.woff2" => "font/woff2",
    "fonts/inter-cyrillic.woff2" => "font/woff2",
    "fonts/sourceserif-latin.woff2" => "font/woff2",
    "fonts/sourceserif-cyrillic.woff2" => "font/woff2",
    "fonts/jbmono-latin.woff2" => "font/woff2",
    "fonts/jbmono-cyrillic.woff2" => "font/woff2",
}

async fn static_file(Path(path): Path<String>) -> Response {
    match asset(&path) {
        Some((bytes, mime)) => (
            [
                (header::CONTENT_TYPE, HeaderValue::from_static(mime)),
                (
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("public, max-age=86400"),
                ),
            ],
            bytes,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
