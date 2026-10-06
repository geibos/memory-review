use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use memory_review::agent::{Agent, AgentCfg};
use memory_review::config::Config;
use memory_review::db::{self, Db};
use memory_review::i18n::strings;
use memory_review::llm::{LiteLlm, model_handle};
use memory_review::memory::McpMemory;
use memory_review::prompts::Prompts;
use memory_review::web::{AppState, router};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        return healthcheck().await;
    }
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cfg = Arc::new(Config::from_env()?);
    let db = Db::open(&cfg.db_path)?;
    let interrupted = db.call(|c| db::reset_interrupted(c)).await?;
    if !interrupted.is_empty() {
        tracing::warn!(
            ?interrupted,
            "cards left with the agent by a previous run were reset"
        );
    }
    let memory = Arc::new(McpMemory::new(cfg.mcp_url.clone(), cfg.project.clone()));
    // The settings page overrides MR_MODEL; MR_MODEL is the default.
    let initial_model = db
        .call(|c| db::get_setting(c, "model"))
        .await?
        .unwrap_or_else(|| cfg.model.clone());
    let model = model_handle(initial_model);
    let llm = Arc::new(LiteLlm::new(
        cfg.llm_url.clone(),
        cfg.llm_key.clone(),
        model.clone(),
    ));
    let prompts = Arc::new(Prompts::load(cfg.prompts_dir.as_deref())?);
    let agent = Agent {
        memory: memory.clone(),
        llm,
        db: db.clone(),
        prompts,
        cfg: AgentCfg {
            inbox_dir: cfg.inbox_dir.clone(),
            verified_dir: cfg.verified_dir.clone(),
        },
    }
    .spawn();

    let state = AppState {
        cfg: Arc::clone(&cfg),
        db,
        memory,
        agent,
        t: strings(cfg.lang),
    };
    let listener = tokio::net::TcpListener::bind(cfg.bind)
        .await
        .with_context(|| format!("binding {}", cfg.bind))?;
    tracing::info!(addr = %cfg.bind, project = %cfg.project, model = %cfg.model, "memory-review listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

/// Resolves on SIGINT or SIGTERM.
async fn shutdown() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!("cannot listen for Ctrl-C: {e}");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::error!("cannot listen for SIGTERM: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { () = ctrl_c => {}, () = term => {} }
    tracing::info!("shutting down");
}

/// `memory-review healthcheck`: exit 0 if the local server answers `/healthz`.
async fn healthcheck() -> anyhow::Result<()> {
    let bind = std::env::var("MR_BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let port = bind.rsplit(':').next().unwrap_or("8080");
    let resp = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?
        .get(format!("http://127.0.0.1:{port}/healthz"))
        .send()
        .await?;
    anyhow::ensure!(
        resp.status().is_success(),
        "healthz returned {}",
        resp.status()
    );
    Ok(())
}
