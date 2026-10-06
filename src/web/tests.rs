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
    setup_with(lang, Some(vec!["m".into(), "other-model".into()]))
}

fn setup_with(lang: &str, catalog: Option<Vec<String>>) -> T {
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
        model: crate::llm::model_handle("m"),
        catalog: Arc::new(crate::llm::fake::FakeCatalog(catalog)),
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
            changes: vec![],
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
    let (s, body, headers) = t.post(&format!("/p/{id}/accept"), "version=1").await;
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

fn model_now(t: &T) -> String {
    crate::llm::current_model(&t.state.model)
}

#[tokio::test]
async fn settings_page_lists_models() {
    let t = setup();
    let (s, page) = t.get("/settings").await;
    assert_eq!(s, StatusCode::OK);
    assert!(page.contains(r#"<option value="other-model""#), "{page}");
    assert!(page.contains(r#"<option value="m" selected"#), "{page}");
    assert!(
        page.contains("http://llm.example.org"),
        "endpoint shown read-only"
    );
    assert!(
        !page.contains(">k<") && !page.contains("value=\"k\""),
        "key never shown"
    );
}

#[tokio::test]
async fn settings_without_catalog_allows_manual_model() {
    let t = setup_with("en", None);
    let (s, page) = t.get("/settings").await;
    assert_eq!(s, StatusCode::OK);
    assert!(page.contains(t.state.t.models_unavailable), "{page}");
    let (s, _, _) = t.post("/settings", "model=&model_manual=custom-1").await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(model_now(&t), "custom-1");
}

#[tokio::test]
async fn settings_post_updates_handle_and_db() {
    let t = setup();
    let (s, _, headers) = t.post("/settings", "model=other-model&model_manual=").await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "/settings?saved=1");
    assert_eq!(model_now(&t), "other-model");
    let stored = t
        .state
        .db
        .call(|c| db::get_setting(c, "model"))
        .await
        .unwrap();
    assert_eq!(stored.as_deref(), Some("other-model"));
}

#[tokio::test]
async fn settings_post_rejects_bad_model() {
    let t = setup();
    let long = "x".repeat(201);
    for form in [
        "model=&model_manual=".to_string(),
        "model=a+b&model_manual=".to_string(),
        format!("model={long}&model_manual="),
    ] {
        let (s, _, _) = t.post("/settings", &form).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{form}");
    }
    assert_eq!(model_now(&t), "m");
}

#[tokio::test]
async fn settings_post_without_origin_forbidden() {
    let t = setup();
    let (s, _, _) = t
        .send(
            Request::post("/settings")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("model=x"))
                .unwrap(),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(model_now(&t), "m");
}

#[tokio::test]
async fn header_shows_model() {
    let t = setup();
    crate::llm::set_model(&t.state.model, "shown-model");
    let (_, h) = t.htmx_get("/header").await;
    assert!(h.contains("shown-model"), "{h}");
    assert!(h.contains(r#"href="/settings""#), "{h}");
}

#[tokio::test]
async fn draft_tab_is_default() {
    let t = setup();
    let a = t.mem.add("inbox", "A", "a");
    let id = t.card(Action::Promote, &[&a]).await;
    let (_, frag) = t.htmx_get(&format!("/p/{id}")).await;
    assert!(frag.contains(r#"id="tab-draft" checked"#), "{frag}");
}

#[tokio::test]
async fn anchored_draft_comment_saved_and_highlighted() {
    let t = setup();
    let a = t.mem.add("inbox", "A", "a");
    let id = t.card(Action::Promote, &[&a]).await;
    let (s, frag, _) = t
        .post(
            &format!("/p/{id}/comment"),
            "body=Too+vague&send=0&anchor=draft&quote=final",
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert!(frag.contains(r#"class="cm""#), "{frag}");
    let m = t.state.db.call(move |c| db::messages(c, id)).await.unwrap();
    assert_eq!(
        m[0].anchor,
        Some(crate::domain::Anchor::Draft {
            version: 1,
            quote: "final".into()
        })
    );
}

#[tokio::test]
async fn anchored_comment_without_quote_rejected() {
    let t = setup();
    let a = t.mem.add("inbox", "A", "a");
    let id = t.card(Action::Promote, &[&a]).await;
    let (s, _, _) = t
        .post(
            &format!("/p/{id}/comment"),
            "body=x&send=0&anchor=draft&quote=",
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn long_quote_truncated_to_500_chars() {
    let t = setup();
    let a = t.mem.add("inbox", "A", "a");
    let id = t.card(Action::Promote, &[&a]).await;
    let q = "q".repeat(700);
    t.post(
        &format!("/p/{id}/comment"),
        &format!("body=x&send=0&anchor=draft&quote={q}"),
    )
    .await;
    let m = t.state.db.call(move |c| db::messages(c, id)).await.unwrap();
    let Some(crate::domain::Anchor::Draft { quote, .. }) = &m[0].anchor else {
        panic!()
    };
    assert_eq!(quote.chars().count(), 500);
}

#[tokio::test]
async fn diff_line_comment_saved_and_shown_under_line() {
    let t = setup();
    let a = t.mem.add("inbox", "A", "a");
    let id = t.card(Action::Promote, &[&a]).await;
    let form = format!("body=Keep+this&send=0&anchor=diff&permalink={a}&line=-+a");
    let (s, frag, _) = t.post(&format!("/p/{id}/comment"), &form).await;
    assert_eq!(s, StatusCode::OK);
    assert!(frag.contains("line-comments"), "{frag}");
    let m = t.state.db.call(move |c| db::messages(c, id)).await.unwrap();
    assert!(matches!(
        m[0].anchor,
        Some(crate::domain::Anchor::Diff { .. })
    ));
}

#[tokio::test]
async fn accept_with_stale_version_is_refused() {
    let t = setup();
    let a = t.mem.add("inbox", "A", "a");
    let id = t.card(Action::Promote, &[&a]).await;
    let (s, frag, _) = t.post(&format!("/p/{id}/accept"), "version=7").await;
    assert_eq!(s, StatusCode::OK);
    assert!(frag.contains(t.state.t.draft_changed), "{frag}");
    assert_eq!(t.status(id).await, Status::Ready);
    assert!(t.mem.raw(&a).is_some());
}

#[tokio::test]
async fn accept_with_current_version_works() {
    let t = setup();
    let a = t.mem.add("inbox", "A", "a");
    let id = t.card(Action::Promote, &[&a]).await;
    t.post(&format!("/p/{id}/accept"), "version=1").await;
    assert_eq!(t.status(id).await, Status::Accepted);
}

#[tokio::test]
async fn static_assets_are_versioned() {
    let t = setup();
    let (_, page) = t.get("/").await;
    assert!(page.contains("/static/app.css?v="), "{page}");
    assert!(page.contains("/static/app.js?v="), "{page}");
    let (_, settings) = t.get("/settings").await;
    assert!(settings.contains("/static/app.css?v="));
}

#[tokio::test]
async fn accept_without_version_is_refused() {
    // A tab opened before versions existed must reload before accepting.
    let t = setup();
    let a = t.mem.add("inbox", "A", "a");
    let id = t.card(Action::Promote, &[&a]).await;
    let (_, frag, _) = t.post(&format!("/p/{id}/accept"), "").await;
    assert!(frag.contains(t.state.t.draft_changed), "{frag}");
    assert_eq!(t.status(id).await, Status::Ready);
}
