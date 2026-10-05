use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

use super::*;
use crate::agent::{Agent, AgentCfg, AgentEvent};
use crate::config::Lang;
use crate::db::{self, NewProposal, SourceRow};
use crate::domain::{Action, Status, ValidatedProposal};
use crate::i18n::strings;
use crate::llm::fake::FakeLlm;
use crate::memory::fake::{FakeMemory, raw_note};
use crate::note::content_hash;
use crate::prompts::Prompts;

const ORIGIN: &str = "https://review.example.org";

struct T {
    state: AppState,
    mem: Arc<FakeMemory>,
    llm: Arc<FakeLlm>,
}

fn config(lang: &str) -> Config {
    let m = [
        ("MR_MCP_URL", "http://memory.example.org/mcp"),
        ("MR_PROJECT", "p"),
        ("MR_LLM_URL", "http://llm.example.org"),
        ("MR_LLM_KEY", "k"),
        ("MR_MODEL", "m"),
        ("MR_PUBLIC_ORIGIN", ORIGIN),
        ("MR_LANG", lang),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    Config::from_map(&m).unwrap()
}

fn setup_lang(lang: &str) -> T {
    let cfg = config(lang);
    let mem: Arc<FakeMemory> = Arc::default();
    let llm: Arc<FakeLlm> = Arc::default();
    let db = Db::open_in_memory().unwrap();
    let agent = Agent {
        memory: mem.clone(),
        llm: llm.clone(),
        db: db.clone(),
        prompts: Arc::new(Prompts::load(None).unwrap()),
        cfg: AgentCfg {
            inbox_dir: "inbox".into(),
            verified_dir: "verified".into(),
        },
    }
    .spawn();
    let t = strings(if lang == "ru" { Lang::Ru } else { Lang::En });
    let state = AppState {
        cfg: Arc::new(cfg),
        db,
        memory: mem.clone(),
        agent,
        t,
    };
    T { state, mem, llm }
}

fn setup() -> T {
    setup_lang("en")
}

impl T {
    async fn card(&self, action: Action, sources: &[&str]) -> i64 {
        let rows: Vec<SourceRow> = sources
            .iter()
            .map(|p| {
                let raw = self.mem.raw(p).unwrap();
                SourceRow {
                    permalink: p.to_string(),
                    title: "Source title".into(),
                    content_hash: content_hash(&raw),
                    original: raw,
                }
            })
            .collect();
        let writes = action != Action::Delete;
        let v = ValidatedProposal {
            action,
            sources: sources.iter().map(|s| s.to_string()).collect(),
            target_dir: writes.then(|| "ops".into()),
            target_title: writes.then(|| "Final title".into()),
            draft: writes.then(|| "- [fact] final <script>x</script>".into()),
            tags: vec![],
            rationale: "Because.".into(),
        };
        self.state
            .db
            .call(move |c| db::insert_proposal(c, &NewProposal { v, sources: rows }))
            .await
            .unwrap()
    }

    async fn status(&self, id: i64) -> Status {
        self.state
            .db
            .call(move |c| db::get_proposal(c, id))
            .await
            .unwrap()
            .unwrap()
            .status
    }

    async fn send(&self, req: Request<Body>) -> (StatusCode, String, axum::http::HeaderMap) {
        let resp = router(self.state.clone()).oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned(), headers)
    }

    async fn get(&self, uri: &str) -> (StatusCode, String) {
        let (s, b, _) = self
            .send(Request::get(uri).body(Body::empty()).unwrap())
            .await;
        (s, b)
    }

    async fn htmx_get(&self, uri: &str) -> (StatusCode, String) {
        let (s, b, _) = self
            .send(
                Request::get(uri)
                    .header("hx-request", "true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        (s, b)
    }

    async fn post(&self, uri: &str, form: &str) -> (StatusCode, String, axum::http::HeaderMap) {
        self.send(
            Request::post(uri)
                .header("origin", ORIGIN)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(form.to_string()))
                .unwrap(),
        )
        .await
    }
}

#[tokio::test]
async fn post_without_or_with_foreign_origin_is_forbidden() {
    let t = setup();
    let a = t.mem.add("inbox", "A", "a");
    let id = t.card(Action::Promote, &[&a]).await;
    let uri = format!("/p/{id}/accept");
    let (s, _, _) = t
        .send(Request::post(&uri).body(Body::empty()).unwrap())
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _, _) = t
        .send(
            Request::post(&uri)
                .header("origin", "https://evil.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(t.status(id).await, Status::Ready);
    assert!(t.mem.raw(&a).is_some());
}

#[tokio::test]
async fn accept_via_http_moves_note() {
    let t = setup();
    let a = t.mem.add("inbox", "A", "a");
    let id = t.card(Action::Promote, &[&a]).await;
    let (s, body, headers) = t.post(&format!("/p/{id}/accept"), "").await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(headers["hx-trigger"], "mr:queue");
    assert!(t.mem.raw("p/verified/ops/final-title").is_some());
    assert!(t.mem.raw(&a).is_none());
    assert_eq!(t.status(id).await, Status::Accepted);
    assert!(body.contains("id=\"card\""));
}

#[tokio::test]
async fn comment_then_send_enqueues_reply() {
    let t = setup();
    let a = t.mem.add("inbox", "A", "a");
    let id = t.card(Action::Promote, &[&a]).await;
    t.llm.push_tool(json!({"reply": "Done."}));
    let mut rx = t.state.agent.events.subscribe();

    let (s, _, _) = t
        .post(&format!("/p/{id}/comment"), "body=Please+shorten&send=0")
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(t.status(id).await, Status::Ready);
    let (_, body, _) = t.post(&format!("/p/{id}/comment"), "body=&send=1").await;
    assert!(body.contains("Please shorten"), "comment shown in thread");

    loop {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            AgentEvent::Finished(_) => break,
            AgentEvent::Failed(e) => panic!("{e}"),
            _ => {}
        }
    }
    assert_eq!(t.status(id).await, Status::Ready);
    let thread = t.state.db.call(move |c| db::messages(c, id)).await.unwrap();
    assert_eq!(thread.last().unwrap().body, "Done.");
}

#[tokio::test]
async fn send_without_pending_comments_is_noop() {
    let t = setup();
    let a = t.mem.add("inbox", "A", "a");
    let id = t.card(Action::Promote, &[&a]).await;
    let (s, _, _) = t.post(&format!("/p/{id}/comment"), "body=&send=1").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(t.status(id).await, Status::Ready);
}

#[tokio::test]
async fn overlong_comment_rejected() {
    let t = setup();
    let a = t.mem.add("inbox", "A", "a");
    let id = t.card(Action::Promote, &[&a]).await;
    let long = "x".repeat(20_001);
    let (s, _, _) = t
        .post(&format!("/p/{id}/comment"), &format!("body={long}&send=0"))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn card_fragment_vs_full_page() {
    let t = setup();
    let a = t.mem.add("inbox", "A", "a");
    let id = t.card(Action::Promote, &[&a]).await;
    let (s, frag) = t.htmx_get(&format!("/p/{id}")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(!frag.contains("<html"));
    assert!(frag.contains("Final title"));
    // htmx wants the event filter right after the event name, before `from:`.
    assert!(
        frag.contains("hx-trigger=\"mr:changed[window.mrCardIdle"),
        "{frag}"
    );
    let (_, page) = t.get(&format!("/p/{id}")).await;
    assert!(page.contains("<html"));
    assert!(page.contains("Final title"));
}

#[tokio::test]
async fn draft_html_is_escaped() {
    let t = setup();
    let a = t.mem.add("inbox", "A", "<script>alert(1)</script>");
    let id = t.card(Action::Promote, &[&a]).await;
    let (_, frag) = t.htmx_get(&format!("/p/{id}")).await;
    assert!(!frag.contains("<script>"), "{frag}");
}

#[tokio::test]
async fn index_lists_queue_and_marks_changed_source_stale() {
    let t = setup();
    let a = t.mem.add("inbox", "A", "a");
    let b = t.mem.add("inbox", "B", "b");
    let fresh = t.card(Action::Promote, &[&a]).await;
    let edited = t.card(Action::Delete, &[&b]).await;
    t.mem.set_raw(&b, raw_note(&b, "B", "edited"));
    let (s, page) = t.get("/").await;
    assert_eq!(s, StatusCode::OK);
    assert!(page.contains("Final title"));
    assert_eq!(t.status(fresh).await, Status::Ready);
    assert_eq!(t.status(edited).await, Status::Stale);
}

#[tokio::test]
async fn header_counts_untriaged_and_triage_enqueues() {
    let t = setup();
    let a = t.mem.add("inbox", "A", "a");
    t.mem.add("inbox", "B", "b");
    t.card(Action::Promote, &[&a]).await;
    let (_, h) = t.htmx_get("/header").await;
    assert!(h.contains("· 1</button>"), "{h}");

    let b = "p/inbox/b".to_string();
    t.llm
        .push_tool(json!({"action": "delete", "sources": [b], "rationale": "junk"}));
    let mut rx = t.state.agent.events.subscribe();
    let (s, _, _) = t.post("/triage", "").await;
    assert_eq!(s, StatusCode::OK);
    loop {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            AgentEvent::Finished(_) => break,
            AgentEvent::Failed(e) => panic!("{e}"),
            _ => {}
        }
    }
    let q = t
        .state
        .db
        .call(|c| db::list_queue(c, db::QueueFilter::Open))
        .await
        .unwrap();
    assert_eq!(q.len(), 2);
}

#[tokio::test]
async fn snooze_and_regenerate_routes() {
    let t = setup();
    let a = t.mem.add("inbox", "A", "a");
    let id = t.card(Action::Promote, &[&a]).await;
    t.post(&format!("/p/{id}/snooze"), "").await;
    assert_eq!(t.status(id).await, Status::Snoozed);
    // Regenerate only applies to stale cards.
    t.post(&format!("/p/{id}/regenerate"), "").await;
    assert_eq!(t.status(id).await, Status::Snoozed);
}

#[tokio::test]
async fn unknown_card_is_404() {
    let t = setup();
    let (s, _) = t.htmx_get("/p/999").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _, _) = t.post("/p/999/accept", "").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn healthz_and_static() {
    let t = setup();
    assert_eq!(t.get("/healthz").await, (StatusCode::OK, "ok".to_string()));
    let (s, css) = t.get("/static/app.css").await;
    assert_eq!(s, StatusCode::OK);
    assert!(css.contains("--mauve"));
    assert_eq!(
        t.get("/static/../Cargo.toml").await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn ru_strings_rendered_when_lang_ru() {
    let t = setup_lang("ru");
    let a = t.mem.add("inbox", "A", "a");
    t.card(Action::Promote, &[&a]).await;
    let (_, page) = t.get("/").await;
    assert!(page.contains("Принять"));
    assert!(page.contains("lang=\"ru\""));
}
