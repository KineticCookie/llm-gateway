//! Integration tests for LLM Gateway
//!
//! These tests require TEST_OPENAI_API_URL environment variable to be set.
//! All tests run against the specified OpenAI-compatible API endpoint.
//!
//! Example usage:
//! ```bash
//! TEST_OPENAI_API_URL=https://api.openai.com cargo test --test integration_test
//! ```
//!
//! Or for a custom endpoint:
//! ```bash
//! TEST_OPENAI_API_URL=http://localhost:8000/v1 cargo test --test integration_test
//! ```

use llm_gateway::config::{CredentialsConfig, ProxyConfig, SchedulerConfig, ServerConfig, TrafficClassConfig, UpstreamConfig};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_stream::StreamExt;
use reqwest::StatusCode;

/// Get the OpenAI API URL from environment variable
/// 
/// Integration tests require TEST_OPENAI_API_URL to be set.
/// Example: TEST_OPENAI_API_URL=https://api.openai.com cargo test --test integration_test
fn get_openai_api_url() -> String {
    std::env::var("TEST_OPENAI_API_URL")
        .expect("TEST_OPENAI_API_URL environment variable must be set for integration tests")
}

/// Helper to create a test configuration
/// 
/// Uses TEST_OPENAI_API_URL environment variable for the upstream URL.
fn create_test_config() -> ProxyConfig {
    let mut classes = HashMap::new();
    classes.insert(
        "test".to_string(),
        TrafficClassConfig {
            weight: 10,
            priority: Some(100),
            min_concurrency: 1,
            max_concurrency: 10,
            max_queue_size: 100,
        },
    );

    let mut api_keys = HashMap::new();
    api_keys.insert("sk-test-key".to_string(), "test".to_string());

    ProxyConfig {
        server: ServerConfig {
            host: "127.0.0.1".to_string(),
            port: 0, // Use 0 to get a random port
        },
        upstream: UpstreamConfig {
            openai_api_url: get_openai_api_url(),
            api_key: None, // Will forward Authorization header from client
        },
        scheduler: SchedulerConfig {
            global_concurrency: 10,
            timeout_seconds: 300,
            classes,
        },
        credentials: CredentialsConfig {
            api_keys,
            default_class: None,
            fallback_class: None,
        },
    }
}


/// Start the gateway server and return its address
async fn start_gateway_server(config: ProxyConfig) -> (String, tokio::task::JoinHandle<anyhow::Result<()>>) {
    use axum::{routing::{get, post}, Router};
    use llm_gateway::handlers::{chat_completions, health_check, metrics_handler, AppState};
    use llm_gateway::queue::ClassBasedScheduler;
    use tower_http::trace::TraceLayer;

    let scheduler = Arc::new(ClassBasedScheduler::new(&config.scheduler).unwrap());
    
    // Spawn dispatch loop
    let scheduler_clone = scheduler.clone();
    tokio::spawn(async move {
        scheduler_clone.dispatch_loop().await;
    });

    let http_client = reqwest::Client::new();
    let state = AppState {
        config: Arc::new(config.clone()),
        scheduler,
        http_client,
    };

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/metrics", get(metrics_handler))
        .route("/v1/chat/completions", post(chat_completions))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = TcpListener::bind(&addr).await.unwrap();
    let actual_addr = listener.local_addr().unwrap();
    let bind_addr = format!("127.0.0.1:{}", actual_addr.port());

    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await?;
        Ok::<(), anyhow::Error>(())
    });

    (bind_addr, handle)
}

#[tokio::test]
async fn test_ordinary_request() {
    // Create and start gateway (requires TEST_OPENAI_API_URL env var)
    let config = create_test_config();
    let (gateway_addr, _gateway_handle) = start_gateway_server(config).await;

    // Give gateway time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Make a non-streaming request
    let client = reqwest::Client::new();
    let request_body = json!({
        "model": "gpt-3.5-turbo",
        "messages": [
            {"role": "user", "content": "Hello, world!"}
        ],
        "stream": false
    });

    let response = client
        .post(&format!("http://{}/v1/chat/completions", gateway_addr))
        .header("Authorization", "Bearer sk-test-key")
        .header("Content-Type", "application/json")
        .json(&request_body)
        .send()
        .await
        .unwrap();

    // Verify response
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().contains_key("x-proxy-request-id"));

    let response_body: Value = response.json().await.unwrap();
    assert_eq!(response_body["object"], "chat.completion");
    assert!(response_body["choices"].is_array());
    assert!(response_body["choices"][0]["message"]["content"].is_string());
    assert!(response_body["usage"].is_object());
}

#[tokio::test]
async fn test_streaming_request() {
    // Create and start gateway (requires TEST_OPENAI_API_URL env var)
    let config = create_test_config();
    let (gateway_addr, _gateway_handle) = start_gateway_server(config).await;

    // Give gateway time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Make a streaming request
    let client = reqwest::Client::new();
    let request_body = json!({
        "model": "gpt-3.5-turbo",
        "messages": [
            {"role": "user", "content": "Hello, world!"}
        ],
        "stream": true
    });

    let response = client
        .post(&format!("http://{}/v1/chat/completions", gateway_addr))
        .header("Authorization", "Bearer sk-test-key")
        .header("Content-Type", "application/json")
        .json(&request_body)
        .send()
        .await
        .unwrap();

    // Verify response
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().contains_key("x-proxy-request-id"));
    
    let content_type = response.headers().get("content-type").unwrap().to_str().unwrap();
    assert!(content_type.contains("text/event-stream"));

    // Read streaming response
    let mut stream = response.bytes_stream();
    let mut chunks = Vec::new();
    let mut buffer = String::new();
    let mut done = false;
    
    while let Some(chunk_result) = stream.next().await {
        if done {
            break;
        }
        
        let chunk = chunk_result.unwrap();
        let chunk_str = String::from_utf8_lossy(&chunk);
        buffer.push_str(&chunk_str);
        
        // Process complete lines (everything except the last potentially incomplete line)
        let lines: Vec<&str> = buffer.lines().collect();
        let last_line = if lines.len() > 1 {
            lines.last().map(|s| s.to_string())
        } else {
            None
        };
        
        let lines_to_process = if lines.len() > 1 {
            &lines[..lines.len() - 1]
        } else {
            &lines[..]
        };
        
        for line in lines_to_process {
            if line.starts_with("data: ") {
                let data = &line[6..];
                if data == "[DONE]" {
                    done = true;
                    break;
                }
                if let Ok(json) = serde_json::from_str::<Value>(data) {
                    chunks.push(json);
                }
            }
        }
        
        // Keep the last line in buffer if it exists
        buffer = last_line.unwrap_or_default();
    }
    
    // Process any remaining buffer
    if !done {
        for line in buffer.lines() {
            if line.starts_with("data: ") {
                let data = &line[6..];
                if data == "[DONE]" {
                    break;
                }
                if let Ok(json) = serde_json::from_str::<Value>(data) {
                    chunks.push(json);
                }
            }
        }
    }

    // Verify we received chunks
    assert!(!chunks.is_empty(), "Should receive at least one chunk");
    
    // Verify first chunk structure
    let first_chunk = &chunks[0];
    assert_eq!(first_chunk["object"], "chat.completion.chunk");
    assert!(first_chunk["choices"].is_array());
}

#[tokio::test]
async fn test_request_without_auth() {
    // Create and start gateway (requires TEST_OPENAI_API_URL env var)
    let config = create_test_config();
    let (gateway_addr, _gateway_handle) = start_gateway_server(config).await;

    // Give gateway time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Make a request without authorization
    let client = reqwest::Client::new();
    let request_body = json!({
        "model": "gpt-3.5-turbo",
        "messages": [
            {"role": "user", "content": "Hello, world!"}
        ]
    });

    let response = client
        .post(&format!("http://{}/v1/chat/completions", gateway_addr))
        .header("Content-Type", "application/json")
        .json(&request_body)
        .send()
        .await
        .unwrap();

    // Should be rejected with 403
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_sse_newline_format() {
    // Create and start gateway (requires TEST_OPENAI_API_URL env var)
    let config = create_test_config();
    let (gateway_addr, _gateway_handle) = start_gateway_server(config).await;

    // Give gateway time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Make a streaming request
    let client = reqwest::Client::new();
    let request_body = json!({
        "model": "gpt-3.5-turbo",
        "messages": [
            {"role": "user", "content": "Say hello"}
        ],
        "stream": true
    });

    let response = client
        .post(&format!("http://{}/v1/chat/completions", gateway_addr))
        .header("Authorization", "Bearer sk-test-key")
        .header("Content-Type", "application/json")
        .json(&request_body)
        .send()
        .await
        .unwrap();

    // Verify response
    assert_eq!(response.status(), StatusCode::OK);
    
    let content_type = response.headers().get("content-type").unwrap().to_str().unwrap();
    assert!(content_type.contains("text/event-stream"));

    // Read raw bytes to check newline format
    let mut stream = response.bytes_stream();
    let mut raw_bytes = Vec::new();
    
    // Collect chunks until we have at least 2 complete SSE events
    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.unwrap();
        raw_bytes.extend_from_slice(&chunk);
        
        let text = String::from_utf8_lossy(&raw_bytes);
        let data_events: Vec<usize> = text.match_indices("data: ").map(|(i, _)| i).collect();
        
        // Need at least 2 events to check the format between them
        if data_events.len() >= 2 {
            let second_data_pos = data_events[1];
            
            // Check the bytes immediately before the second "data: "
            // SSE format requires \n\n before each new event
            let bytes_before_second = if second_data_pos >= 2 {
                &raw_bytes[second_data_pos - 2..second_data_pos]
            } else {
                &raw_bytes[..second_data_pos]
            };
            
            // Should be exactly [0x0A, 0x0A] which is "\n\n"
            assert_eq!(
                bytes_before_second,
                b"\n\n",
                "Expected \\n\\n (0x0A 0x0A) before second SSE event, but found: {:?}. \
                 Position: {}, Bytes (hex): {:?}",
                bytes_before_second,
                second_data_pos,
                bytes_before_second.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>()
            );
            
            // Also verify subsequent events have proper format
            if data_events.len() >= 3 {
                let third_data_pos = data_events[2];
                let bytes_before_third = if third_data_pos >= 2 {
                    &raw_bytes[third_data_pos - 2..third_data_pos]
                } else {
                    &raw_bytes[..third_data_pos]
                };
                
                assert_eq!(
                    bytes_before_third,
                    b"\n\n",
                    "Expected \\n\\n before third SSE event, but found: {:?}",
                    bytes_before_third.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>()
                );
            }
            
            break; // We've verified the format
        }
    }
    
    assert!(
        raw_bytes.len() > 0,
        "No data received from streaming response"
    );
}

