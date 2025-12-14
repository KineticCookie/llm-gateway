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
use crate::queue::PriorityQueue;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "openai_api_proxy=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Load configuration
    let config = ProxyConfig::from_file("config.yaml")
        .or_else(|_| ProxyConfig::from_env())
        .unwrap_or_else(|e| {
            tracing::warn!("Failed to load config: {}. Using defaults.", e);
            ProxyConfig {
                server: config::ServerConfig {
                    host: "0.0.0.0".to_string(),
                    port: 8080,
                },
                upstream: config::UpstreamConfig {
                    openai_api_url: "https://api.openai.com".to_string(),
                    api_key: None,
                },
                queue: config::QueueConfig {
                    max_queue_size: 100,
                    timeout_seconds: 300,
                    concurrent_limit: 10,
                },
            }
        });

    tracing::info!("Configuration loaded: {:?}", config);

    // Create priority queue
    let queue = Arc::new(PriorityQueue::new(
        config.queue.max_queue_size,
        config.queue.concurrent_limit,
    ));

    // Spawn queue processor tasks
    let queue_clone = queue.clone();
    tokio::spawn(async move {
        loop {
            queue_clone.process_next().await;
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
    });

    // Spawn periodic queue distribution updater
    let queue_clone = queue.clone();
    tokio::spawn(async move {
        loop {
            queue_clone.update_queue_distribution().await;
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }
    });

    // Create HTTP client
    let http_client = reqwest::Client::new();

    // Create application state
    let state = AppState {
        config: Arc::new(config.clone()),
        queue,
        http_client,
    };

    // Build router
    let app = Router::new()
        .route("/health", get(health_check))
        .route("/metrics", get(metrics_handler))
        .route("/v1/chat/completions", post(chat_completions))
        // Add other OpenAI endpoints as pass-through if needed
        // .route("/v1/*path", post(proxy_request))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    // Start server
    let addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    
    tracing::info!("OpenAI API Proxy listening on {}", addr);
    tracing::info!("Queue configuration: max_size={}, concurrent_limit={}", 
        config.queue.max_queue_size, config.queue.concurrent_limit);

    axum::serve(listener, app).await?;

    Ok(())
}
