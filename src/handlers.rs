use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;

use crate::config::ProxyConfig;
use crate::metrics;
use crate::queue::{Priority, PriorityQueue, QueueError};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ProxyConfig>,
    pub queue: Arc<PriorityQueue>,
    pub http_client: reqwest::Client,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    #[serde(flatten)]
    pub openai_params: Value,
    
    /// Optional priority parameter (extension to OpenAI API)
    /// Higher values = higher priority
    /// Default is 0 if not specified
    #[serde(default)]
    pub priority: i32,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: ErrorDetail,
}

#[derive(Debug, Serialize)]
pub struct ErrorDetail {
    pub message: String,
    pub r#type: String,
    pub code: Option<String>,
}

impl IntoResponse for QueueError {
    fn into_response(self) -> Response {
        let (status, message, error_type) = match self {
            QueueError::PriorityTooLow => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Queue is full and request priority is too low".to_string(),
                "queue_full".to_string(),
            ),
            QueueError::Evicted => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Request was evicted from queue by higher priority request".to_string(),
                "evicted".to_string(),
            ),
            QueueError::Timeout => (
                StatusCode::GATEWAY_TIMEOUT,
                "Request timeout".to_string(),
                "timeout".to_string(),
            ),
        };

        let error_response = ErrorResponse {
            error: ErrorDetail {
                message,
                r#type: error_type.clone(),
                code: Some(error_type),
            },
        };

        (status, Json(error_response)).into_response()
    }
}

/// Handler for POST /v1/chat/completions
pub async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut payload): Json<ChatCompletionRequest>,
) -> Result<Response, QueueError> {
    let priority = Priority(payload.priority);
    let request_start = Instant::now();
    let priority_str = priority.0.to_string();
    
    // Record incoming request
    metrics::REQUESTS_TOTAL.with_label_values(&[&priority_str, "received"]).inc();
    
    tracing::info!(
        "Received chat completion request: priority={}, model={}",
        priority.0,
        payload.openai_params.get("model").and_then(|v| v.as_str()).unwrap_or("unknown")
    );
    
    // Remove our custom priority field before forwarding to OpenAI
    if let Value::Object(ref mut map) = payload.openai_params {
        map.remove("priority");
    }

    // Enqueue the request
    tracing::debug!("Enqueueing request with priority={}", priority.0);
    let enqueue_start = Instant::now();
    let rx = state.queue.enqueue(priority).await?;
    
    // Wait for permission to proceed
    tracing::debug!("Waiting for queue permission with priority={}", priority.0);
    rx.await.map_err(|_| QueueError::Timeout)??;
    let queue_duration = enqueue_start.elapsed();
    
    // Record queue metrics
    metrics::QUEUE_DURATION
        .with_label_values(&[&priority_str])
        .observe(queue_duration.as_secs_f64());
    
    tracing::info!(
        "Request dequeued and ready to process: priority={}, queue_time_ms={}",
        priority.0, queue_duration.as_millis()
    );
    
    // Acquire semaphore permit for actual processing
    let _permit = state.queue.acquire_permit().await;

    // Forward request to upstream OpenAI
    tracing::debug!("Forwarding request to upstream OpenAI");
    let openai_start = Instant::now();
    let upstream_url = format!("{}/v1/chat/completions", state.config.upstream.openai_api_url);
    
    let mut request = state.http_client
        .post(&upstream_url)
        .json(&payload.openai_params);

    // Forward authorization header if present, or use configured API key
    if let Some(auth) = headers.get("authorization") {
        request = request.header("authorization", auth);
    } else if let Some(api_key) = &state.config.upstream.api_key {
        request = request.header("authorization", format!("Bearer {}", api_key));
    }

    // Forward other relevant headers
    for (name, value) in headers.iter() {
        let name_str = name.as_str();
        if name_str.starts_with("x-") || name_str == "content-type" {
            request = request.header(name, value);
        }
    }

    let response = request.send().await
        .map_err(|e| {
            tracing::error!("Failed to forward request to OpenAI: {}", e);
            QueueError::Timeout
        })?;

    let status = response.status();
    let response_headers = response.headers().clone();
    let body = response.bytes().await
        .map_err(|_| QueueError::Timeout)?;
    
    let openai_duration = openai_start.elapsed();
    let total_duration = request_start.elapsed();
    
    let status_label = status.as_u16().to_string();
    
    // Record metrics
    metrics::OPENAI_DURATION
        .with_label_values(&[&status_label])
        .observe(openai_duration.as_secs_f64());
    
    metrics::REQUEST_DURATION
        .with_label_values(&[&priority_str, &status_label])
        .observe(total_duration.as_secs_f64());
    
    metrics::REQUESTS_TOTAL.with_label_values(&[&priority_str, "completed"]).inc();
    metrics::REQUESTS_OUTCOME.with_label_values(&["success"]).inc();

    tracing::info!(
        "Request completed: priority={}, status={}, body_size={} bytes, \
         queue_time_ms={}, openai_time_ms={}, total_time_ms={}",
        priority.0, status, body.len(),
        queue_duration.as_millis(),
        openai_duration.as_millis(),
        total_duration.as_millis()
    );

    // Build response
    let mut builder = axum::http::Response::builder().status(status);
    
    // Copy relevant response headers
    for (name, value) in response_headers.iter() {
        let name_str = name.as_str();
        if name_str == "content-type" || name_str.starts_with("x-") || name_str == "openai-" {
            builder = builder.header(name, value);
        }
    }

    Ok(builder.body(axum::body::Body::from(body)).unwrap())
}

/// Handler for other OpenAI API endpoints (pass-through without queuing)
pub async fn proxy_request(
    State(state): State<AppState>,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, StatusCode> {
    let upstream_url = format!("{}/{}", state.config.upstream.openai_api_url, path);
    
    let mut request = state.http_client
        .post(&upstream_url)
        .body(body.to_vec());

    // Forward authorization header
    if let Some(auth) = headers.get("authorization") {
        request = request.header("authorization", auth);
    } else if let Some(api_key) = &state.config.upstream.api_key {
        request = request.header("authorization", format!("Bearer {}", api_key));
    }

    // Forward other headers
    for (name, value) in headers.iter() {
        let name_str = name.as_str();
        if name_str.starts_with("x-") || name_str == "content-type" {
            request = request.header(name, value);
        }
    }

    let response = request.send().await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    let status = response.status();
    let response_headers = response.headers().clone();
    let body = response.bytes().await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    let mut builder = axum::http::Response::builder().status(status);
    
    for (name, value) in response_headers.iter() {
        builder = builder.header(name, value);
    }

    Ok(builder.body(axum::body::Body::from(body)).unwrap())
}

/// Health check endpoint
pub async fn health_check(State(state): State<AppState>) -> Json<Value> {
    let queue_size = state.queue.queue_size().await;
    
    Json(serde_json::json!({
        "status": "ok",
        "queue_size": queue_size,
        "max_queue_size": state.config.queue.max_queue_size
    }))
}

/// Prometheus metrics endpoint
pub async fn metrics_handler() -> Result<String, StatusCode> {
    metrics::encode_metrics().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
