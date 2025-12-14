mod config;
mod handlers;
mod metrics;
mod queue;

use axum::{
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::config::ProxyConfig;
use crate::handlers::{chat_completions, health_check, metrics_handler, AppState};
use crate::queue::ClassBasedScheduler;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "llm_gateway=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Load configuration
    let config = ProxyConfig::from_file("config.yaml").or_else(|_| ProxyConfig::from_env())?;

    tracing::info!("Configuration loaded successfully");
    tracing::info!("Server: {}:{}", config.server.host, config.server.port);
    tracing::info!(
        "Global concurrency: {}",
        config.scheduler.global_concurrency
    );

    // Create class-based scheduler
    let scheduler = Arc::new(ClassBasedScheduler::new(&config.scheduler)?);

    // Log credential class mappings
    if let Some(default_class) = &config.credentials.default_class {
        tracing::info!("Default class for unknown credentials: {}", default_class);
    } else {
        tracing::info!("Default class for unknown credentials: disabled (403 Forbidden)");
    }

    if let Some(fallback_class) = &config.credentials.fallback_class {
        tracing::info!(
            "Fallback class for misconfigured mappings: {}",
            fallback_class
        );
    } else {
        tracing::info!("Fallback class for misconfigured mappings: disabled (403 Forbidden)");
    }

    // Spawn dispatch loop
    let scheduler_clone = scheduler.clone();
    tokio::spawn(async move {
        scheduler_clone.dispatch_loop().await;
    });

    // Create HTTP client
    let http_client = reqwest::Client::new();

    // Create application state
    let state = AppState {
        config: Arc::new(config.clone()),
        scheduler,
        http_client,
    };

    // Build router
    let app = Router::new()
        .route("/health", get(health_check))
        .route("/status", get(handlers::status))
        .route("/metrics", get(metrics_handler))
        .route("/v1/chat/completions", post(chat_completions))
        // Add other OpenAI endpoints as pass-through if needed
        // .route("/v1/*path", post(proxy_request))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    // Start server
    let addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    tracing::info!("Listening on {}", addr);
    tracing::info!(
        "Class-based scheduling active with {} traffic classes",
        config.scheduler.classes.len()
    );

    axum::serve(listener, app).await?;

    Ok(())
}
