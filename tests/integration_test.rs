//! Integration tests for LLM Gateway
//!
//! These tests require TEST_OPENAI_API_URL environment variable to be set.
//!
//! Example usage:
//! ```bash
//! TEST_OPENAI_API_URL=http://localhost:8000 cargo test --test integration_test
//! ```

use llm_gateway::config::{ProjectConfig, ProxyConfig, ServerConfig, UnauthenticatedPolicy, RejectLiteral, UpstreamConfig};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_stream::StreamExt;
use reqwest::StatusCode;

fn get_upstream_url() -> String {
    std::env::var("TEST_OPENAI_API_URL")
        .expect("TEST_OPENAI_API_URL must be set for integration tests")
}

fn create_test_config() -> Arc<ProxyConfig> {
    let mut projects = HashMap::new();
    projects.insert(
        "test".to_string(),
        ProjectConfig {
            priority: 1,
            share: 1,
            max_slots: Some(10),
            timeout: Some(Duration::from_secs(30)),
            api_keys: vec!["sk-test-key".to_string()],
        },
    );

    Arc::new(ProxyConfig {
        server: ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 0,
        },
        upstream: UpstreamConfig {
            url: get_upstream_url(),
            api_key: None,
        },
        slots: 10,
        default_timeout: Duration::from_secs(30),
        unauthenticated: UnauthenticatedPolicy::Reject(RejectLiteral::Reject),
        projects,
    })
}

async fn start_gateway(config: Arc<ProxyConfig>) -> String {
    use axum::{routing::{get, post}, Router};
    use llm_gateway::handlers::{chat_completions, health_check, metrics_handler, AppState};
    use llm_gateway::queue::ClassBasedScheduler;
    use tower_http::trace::TraceLayer;

    let scheduler = Arc::new(ClassBasedScheduler::new(Arc::clone(&config)));

    let scheduler_clone = Arc::clone(&scheduler);
    tokio::spawn(async move { scheduler_clone.dispatch_loop().await });

    let state = AppState {
        config: Arc::clone(&config),
        scheduler,
        http_client: reqwest::Client::new(),
    };

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/metrics", get(metrics_handler))
        .route("/v1/chat/completions", post(chat_completions))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());

    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(50)).await;

    addr
}

#[tokio::test]
async fn test_sync_request() {
    let addr = start_gateway(create_test_config()).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{}/v1/chat/completions", addr))
        .header("Authorization", "Bearer sk-test-key")
        .header("Content-Type", "application/json")
        .json(&json!({
            "model": "gpt-3.5-turbo",
            "messages": [{"role": "user", "content": "Hello"}],
            "stream": false
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().contains_key("x-request-id"));

    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["object"], "chat.completion");
    assert!(body["choices"][0]["message"]["content"].is_string());
}

#[tokio::test]
async fn test_streaming_request() {
    let addr = start_gateway(create_test_config()).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{}/v1/chat/completions", addr))
        .header("Authorization", "Bearer sk-test-key")
        .header("Content-Type", "application/json")
        .json(&json!({
            "model": "gpt-3.5-turbo",
            "messages": [{"role": "user", "content": "Hello"}],
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().contains_key("x-request-id"));
    assert!(resp.headers().get("content-type").unwrap().to_str().unwrap().contains("text/event-stream"));

    let mut stream = resp.bytes_stream();
    let mut chunks: Vec<Value> = vec![];

    while let Some(Ok(chunk)) = stream.next().await {
        for line in String::from_utf8_lossy(&chunk).lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                if data == "[DONE]" { break; }
                if let Ok(v) = serde_json::from_str::<Value>(data) {
                    chunks.push(v);
                }
            }
        }
    }

    assert!(!chunks.is_empty());
    assert_eq!(chunks[0]["object"], "chat.completion.chunk");
}

#[tokio::test]
async fn test_no_auth_rejected() {
    let addr = start_gateway(create_test_config()).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{}/v1/chat/completions", addr))
        .header("Content-Type", "application/json")
        .json(&json!({"model": "gpt-3.5-turbo", "messages": [{"role": "user", "content": "Hi"}]}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_unknown_key_rejected() {
    let addr = start_gateway(create_test_config()).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{}/v1/chat/completions", addr))
        .header("Authorization", "Bearer sk-wrong-key")
        .header("Content-Type", "application/json")
        .json(&json!({"model": "gpt-3.5-turbo", "messages": [{"role": "user", "content": "Hi"}]}))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_sse_newline_format() {
    let addr = start_gateway(create_test_config()).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{}/v1/chat/completions", addr))
        .header("Authorization", "Bearer sk-test-key")
        .header("Content-Type", "application/json")
        .json(&json!({
            "model": "gpt-3.5-turbo",
            "messages": [{"role": "user", "content": "Say hello"}],
            "stream": true
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let mut stream = resp.bytes_stream();
    let mut raw = Vec::new();

    while let Some(Ok(chunk)) = stream.next().await {
        raw.extend_from_slice(&chunk);
        let text = String::from_utf8_lossy(&raw);
        let positions: Vec<usize> = text.match_indices("data: ").map(|(i, _)| i).collect();
        if positions.len() >= 2 {
            let before = &raw[positions[1] - 2..positions[1]];
            assert_eq!(before, b"\n\n", "SSE events must be separated by \\n\\n");
            break;
        }
    }

    assert!(!raw.is_empty());
}
