use axum::{
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use llm_gateway::config::ProxyConfig;
use llm_gateway::handlers::{chat_completions, health_check, metrics_handler, status, AppState};
use llm_gateway::queue::ClassBasedScheduler;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "llm_gateway=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config =
        Arc::new(ProxyConfig::from_file("config.yaml").or_else(|_| ProxyConfig::from_env())?);

    tracing::info!(
        host = %config.server.host,
        port = config.server.port,
        slots = config.slots,
        projects = config.projects.len(),
        "configuration loaded"
    );

    for (name, project) in &config.projects {
        tracing::info!(
            project = %name,
            priority = project.priority,
            share = project.share,
            max_slots = ?project.max_slots,
            "project configured"
        );
    }

    let scheduler = Arc::new(ClassBasedScheduler::new(Arc::clone(&config)));

    let scheduler_clone = Arc::clone(&scheduler);
    tokio::spawn(async move {
        scheduler_clone.dispatch_loop().await;
    });

    let http_client = reqwest::Client::new();

    let state = AppState {
        config: Arc::clone(&config),
        scheduler,
        http_client,
    };

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/status", get(status))
        .route("/metrics", get(metrics_handler))
        .route("/v1/chat/completions", post(chat_completions))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(addr = %addr, "listening");

    axum::serve(listener, app).await?;
    Ok(())
}
