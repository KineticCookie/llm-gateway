use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use rand::Rng;
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

use crate::config::ProxyConfig;
use crate::metrics;
use crate::queue::{ClassBasedScheduler, QueueError, TrafficClass};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ProxyConfig>,
    pub scheduler: Arc<ClassBasedScheduler>,
    pub http_client: reqwest::Client,
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
        let (status, message, error_type, class_opt) = match self {
            QueueError::QueueFull(class) => (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Queue is full for traffic class: {}", class),
                "queue_full".to_string(),
                Some(class),
            ),
            QueueError::Evicted(class) => (
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "Request evicted due to higher priority traffic (class: {})",
                    class
                ),
                "evicted".to_string(),
                Some(class),
            ),
            QueueError::Timeout(class) => (
                StatusCode::GATEWAY_TIMEOUT,
                format!(
                    "Request timeout (total lifetime exceeded) for class {}",
                    class
                ),
                "timeout".to_string(),
                Some(class),
            ),
            QueueError::UnknownClass(class) => (
                StatusCode::FORBIDDEN,
                format!("Unknown traffic class: {}", class),
                "unknown_class".to_string(),
                None,
            ),
        };

        let error_response = ErrorResponse {
            error: ErrorDetail {
                message,
                r#type: error_type.clone(),
                code: Some(error_type),
            },
        };

        let mut response = (status, Json(error_response)).into_response();

        // Add Retry-After header for retryable errors
        if let Some(class_name) = class_opt {
            let traffic_class = TrafficClass::new(class_name);
            let retry_after = calculate_retry_after_seconds(&traffic_class);
            response
                .headers_mut()
                .insert("retry-after", retry_after.to_string().parse().unwrap());
        }

        response
    }
}

/// Calculate retry-after seconds with jitter based on p95 queue wait time
fn calculate_retry_after_seconds(traffic_class: &TrafficClass) -> u64 {
    let mut rng = rand::thread_rng();

    // Get the p95 queue wait time from the histogram
    let p95_seconds = metrics::QUEUE_DURATION
        .get_metric_with_label_values(&[traffic_class.as_str()])
        .ok()
        .and_then(|metric| {
            // Estimate p95 from histogram buckets
            let histogram = metric.get_sample_sum();
            let count = metric.get_sample_count();
            if count > 0 {
                // Use average as a fallback approximation if we can't calculate exact p95
                // In practice, Prometheus client doesn't expose bucket counts directly,
                // so we use sum/count as a reasonable estimate
                Some((histogram / count as f64).ceil() as u64)
            } else {
                None
            }
        })
        .unwrap_or(2); // Default to 2 seconds if no data

    let clamped = p95_seconds.clamp(1, 10);
    let jitter = rng.gen_range(0..=2); // 0-2 seconds jitter
    clamped + jitter
}

/// Extract traffic class from authorization header
fn extract_traffic_class(
    headers: &HeaderMap,
    config: &ProxyConfig,
) -> Result<TrafficClass, StatusCode> {
    // Extract API key from Authorization header
    let api_key = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .or_else(|| {
            // Fallback: try x-api-key header
            headers.get("x-api-key").and_then(|h| h.to_str().ok())
        });

    let Some(api_key) = api_key else {
        tracing::warn!("Request without API key");
        metrics::UNKNOWN_CREDENTIALS
            .with_label_values(&["no_key"])
            .inc();

        // Check if we have a default class for unknown credentials
        if let Some(default_class) = &config.credentials.default_class {
            if config.scheduler.classes.contains_key(default_class) {
                return Ok(TrafficClass::new(default_class.clone()));
            }
        }
        return Err(StatusCode::FORBIDDEN);
    };

    // Map API key to traffic class
    if let Some(class_name) = config.credentials.api_keys.get(api_key) {
        // Verify the class exists in scheduler config
        if config.scheduler.classes.contains_key(class_name) {
            return Ok(TrafficClass::new(class_name.clone()));
        } else {
            tracing::error!(
                "API key mapped to invalid class '{}' (not in scheduler config)",
                class_name
            );
            metrics::UNKNOWN_CREDENTIALS
                .with_label_values(&["invalid_class"])
                .inc();

            // Try fallback class for misconfigured mappings
            if let Some(fallback) = &config.credentials.fallback_class {
                if config.scheduler.classes.contains_key(fallback) {
                    tracing::warn!(
                        "Using fallback class '{}' for misconfigured mapping",
                        fallback
                    );
                    return Ok(TrafficClass::new(fallback.clone()));
                }
            }
            return Err(StatusCode::FORBIDDEN);
        }
    }

    // Unknown API key
    tracing::warn!("Unknown API key: {}...", &api_key[..api_key.len().min(8)]);
    metrics::UNKNOWN_CREDENTIALS
        .with_label_values(&["unknown_key"])
        .inc();

    // Check if we have a default class
    if let Some(default_class) = &config.credentials.default_class {
        if config.scheduler.classes.contains_key(default_class) {
            return Ok(TrafficClass::new(default_class.clone()));
        }
    }

    Err(StatusCode::FORBIDDEN)
}

/// Handler for POST /v1/chat/completions
pub async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, StatusCode> {
    let request_start = Instant::now();
    let request_id = Uuid::new_v4().to_string();

    // Extract traffic class from credentials
    let traffic_class = extract_traffic_class(&headers, &state.config).map_err(|status| {
        if status == StatusCode::FORBIDDEN {
            tracing::warn!("Rejecting request with unknown credentials");
        }
        status
    })?;

    let class_str = traffic_class.as_str().to_string();

    // Record incoming request
    metrics::REQUESTS_TOTAL
        .with_label_values(&[&class_str, "received"])
        .inc();

    tracing::info!(
        "Received chat completion request: class={}, body_size={}",
        class_str,
        body.len()
    );

    // Enqueue the request
    tracing::debug!(
        "Enqueueing request with class={}, request_id={}",
        class_str,
        request_id
    );
    let enqueue_start = Instant::now();
    let rx = state
        .scheduler
        .enqueue(traffic_class.clone())
        .await
        .map_err(|e| {
            let mut response = match e {
                QueueError::QueueFull(_) => {
                    metrics::REQUESTS_OUTCOME
                        .with_label_values(&["rejected"])
                        .inc();
                    StatusCode::SERVICE_UNAVAILABLE.into_response()
                }
                _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            };

            // Add Retry-After header for retryable errors
            if matches!(e, QueueError::QueueFull(_)) {
                let retry_after = calculate_retry_after_seconds(&traffic_class);
                response
                    .headers_mut()
                    .insert("retry-after", retry_after.to_string().parse().unwrap());
            }

            // Always add request ID
            response
                .headers_mut()
                .insert("x-proxy-request-id", request_id.parse().unwrap());

            StatusCode::SERVICE_UNAVAILABLE
        })?;

    // Wait for permission to proceed with timeout
    tracing::debug!(
        "Waiting for queue permission with class={}, request_id={}",
        class_str,
        request_id
    );
    state
        .scheduler
        .wait_with_timeout(traffic_class.clone(), rx)
        .await
        .map_err(|e| {
            let mut response = match e {
                QueueError::Evicted(_) => {
                    metrics::REQUESTS_OUTCOME
                        .with_label_values(&["evicted"])
                        .inc();
                    StatusCode::SERVICE_UNAVAILABLE.into_response()
                }
                QueueError::Timeout(_) => {
                    metrics::REQUESTS_OUTCOME
                        .with_label_values(&["timeout"])
                        .inc();
                    StatusCode::GATEWAY_TIMEOUT.into_response()
                }
                _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            };

            // Retry-After is already added by QueueError::into_response()

            // Always add request ID
            response
                .headers_mut()
                .insert("x-proxy-request-id", request_id.parse().unwrap());

            if matches!(e, QueueError::Timeout(_)) {
                StatusCode::GATEWAY_TIMEOUT
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            }
        })?;

    let queue_duration = enqueue_start.elapsed();

    tracing::info!(
        "Request dequeued and ready to process: class={}, queue_time_ms={}",
        class_str,
        queue_duration.as_millis()
    );

    // Acquire permit for actual processing
    let _permit = state.scheduler.acquire_permit(traffic_class.clone()).await;

    // Forward request to upstream OpenAI
    tracing::debug!("Forwarding request to upstream OpenAI");
    let openai_start = Instant::now();
    let upstream_url = format!(
        "{}/v1/chat/completions",
        state.config.upstream.openai_api_url
    );

    let mut request = state.http_client.post(&upstream_url).body(body);

    // Forward authorization header if present, or use configured API key
    if let Some(auth) = headers.get("authorization") {
        request = request.header("authorization", auth);
    } else if let Some(api_key) = &state.config.upstream.api_key {
        request = request.header("authorization", format!("Bearer {}", api_key));
    }

    // Forward content-type and other relevant headers
    for (name, value) in headers.iter() {
        let name_str = name.as_str();
        if name_str.starts_with("x-") || name_str == "content-type" {
            request = request.header(name, value);
        }
    }

    let response = request.send().await.map_err(|e| {
        tracing::error!("Failed to forward request to OpenAI: {}", e);
        StatusCode::BAD_GATEWAY
    })?;

    let status = response.status();
    let response_headers = response.headers().clone();
    let body = response
        .bytes()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    let openai_duration = openai_start.elapsed();
    let total_duration = request_start.elapsed();

    let status_label = status.as_u16().to_string();

    // Record metrics
    metrics::OPENAI_DURATION
        .with_label_values(&[&class_str, &status_label])
        .observe(openai_duration.as_secs_f64());

    metrics::REQUEST_DURATION
        .with_label_values(&[&class_str, &status_label])
        .observe(total_duration.as_secs_f64());

    // Count upstream OpenAI response status codes for easier querying
    metrics::OPENAI_RESPONSES
        .with_label_values(&[&class_str, &status_label])
        .inc();

    metrics::REQUESTS_TOTAL
        .with_label_values(&[&class_str, "completed"])
        .inc();
    metrics::REQUESTS_OUTCOME
        .with_label_values(&["success"])
        .inc();

    tracing::info!(
        "Request completed: class={}, status={}, body_size={} bytes, \
         queue_time_ms={}, openai_time_ms={}, total_time_ms={}, request_id={}",
        class_str,
        status,
        body.len(),
        queue_duration.as_millis(),
        openai_duration.as_millis(),
        total_duration.as_millis(),
        request_id
    );

    // Build response
    let mut builder = axum::http::Response::builder()
        .status(status)
        .header("x-proxy-request-id", &request_id);

    // Copy relevant response headers
    for (name, value) in response_headers.iter() {
        let name_str = name.as_str();
        if name_str == "content-type"
            || name_str.starts_with("x-")
            || name_str.starts_with("openai-")
        {
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
    // Still check credentials even for pass-through
    let traffic_class = extract_traffic_class(&headers, &state.config)?;
    let class_str = traffic_class.as_str().to_string();

    let upstream_url = format!("{}/{}", state.config.upstream.openai_api_url, path);

    let mut request = state.http_client.post(&upstream_url).body(body.to_vec());

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

    let response = request.send().await.map_err(|_| StatusCode::BAD_GATEWAY)?;

    let status = response.status();
    let response_headers = response.headers().clone();
    let body = response
        .bytes()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    let status_label = status.as_u16().to_string();

    // Count upstream OpenAI response status codes for pass-through requests
    metrics::OPENAI_RESPONSES
        .with_label_values(&[&class_str, &status_label])
        .inc();

    let mut builder = axum::http::Response::builder().status(status);

    for (name, value) in response_headers.iter() {
        builder = builder.header(name, value);
    }

    Ok(builder.body(axum::body::Body::from(body)).unwrap())
}

/// Health check endpoint

pub async fn health_check() -> StatusCode {
    StatusCode::OK
}

pub async fn status(State(state): State<AppState>) -> Json<Value> {
    let mut class_info = serde_json::Map::new();

    // Get all configured classes
    for class_name in state.config.scheduler.classes.keys() {
        let class = TrafficClass::new(class_name.clone());
        class_info.insert(
            class_name.clone(),
            serde_json::json!({
                "queue_size": state.scheduler.queue_size(class.clone()).await,
                "in_flight": state.scheduler.in_flight_count(class.clone()).await,
            }),
        );
    }

    Json(serde_json::json!({
        "status": "ok",
        "classes": class_info,
    }))
}

/// Prometheus metrics endpoint
pub async fn metrics_handler() -> Result<String, StatusCode> {
    metrics::encode_metrics().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
