use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response, Sse},
    Json,
};
use futures::StreamExt as FuturesStreamExt;
use serde::Serialize;
use serde_json::Value;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;
use tokio_stream::Stream;
use uuid::Uuid;

use crate::queue::SlotPermit;

/// Wraps a stream and holds a SlotPermit for its lifetime.
/// The slot is only released when this stream is dropped (i.e. when streaming completes).
struct PermitStream<S> {
    inner: S,
    _permit: SlotPermit,
}

impl<S: Stream + Unpin> Stream for PermitStream<S> {
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

use crate::config::ProxyConfig;
use crate::metrics;
use crate::queue::{ClassBasedScheduler, QueueError};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ProxyConfig>,
    pub scheduler: Arc<ClassBasedScheduler>,
    pub http_client: reqwest::Client,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: ErrorDetail,
}

#[derive(Debug, Serialize)]
struct ErrorDetail {
    message: String,
    r#type: String,
}

fn error_response(status: StatusCode, error_type: &str, message: String) -> Response {
    let body = ErrorResponse {
        error: ErrorDetail {
            message,
            r#type: error_type.to_string(),
        },
    };
    (status, Json(body)).into_response()
}

/// Extract project name from Authorization or x-api-key header.
fn resolve_project<'a>(
    headers: &HeaderMap,
    config: &'a ProxyConfig,
) -> Result<&'a str, Box<Response>> {
    let api_key = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-api-key").and_then(|h| h.to_str().ok()));

    match api_key {
        None => {
            metrics::UNKNOWN_CREDENTIALS
                .with_label_values(&["no_key"])
                .inc();

            match config.unauthenticated.project_name() {
                Some(name) => Ok(name),
                None => Err(Box::new(error_response(
                    StatusCode::FORBIDDEN,
                    "no_credentials",
                    "No API key provided".to_string(),
                ))),
            }
        }
        Some(key) => match config.project_for_key(key) {
            Some(name) => Ok(name),
            None => {
                metrics::UNKNOWN_CREDENTIALS
                    .with_label_values(&["unknown_key"])
                    .inc();
                tracing::warn!("Unknown API key: {}...", &key[..key.len().min(8)]);

                match config.unauthenticated.project_name() {
                    Some(name) => Ok(name),
                    None => Err(Box::new(error_response(
                        StatusCode::FORBIDDEN,
                        "unknown_key",
                        "Unknown API key".to_string(),
                    ))),
                }
            }
        },
    }
}

pub async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let request_start = Instant::now();
    let request_id = Uuid::new_v4().to_string();

    let project_name = match resolve_project(&headers, &state.config) {
        Ok(p) => p.to_string(),
        Err(resp) => return *resp,
    };

    tracing::info!(
        request_id = %request_id,
        project = %project_name,
        body_bytes = body.len(),
        "received chat completion request"
    );

    // Enqueue — fails immediately if queue is full
    let rx = match state.scheduler.enqueue(&project_name).await {
        Ok(rx) => rx,
        Err(QueueError::QueueFull(_)) => {
            metrics::REQUESTS_TOTAL
                .with_label_values(&[&project_name, "rejected"])
                .inc();
            let mut resp = error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "queue_full",
                format!("Queue is full for project: {}", project_name),
            );
            resp.headers_mut()
                .insert("retry-after", "5".parse().unwrap());
            resp.headers_mut()
                .insert("x-request-id", request_id.parse().unwrap());
            return resp;
        }
        Err(e) => {
            tracing::error!("Unexpected enqueue error: {}", e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Wait for a slot — may be evicted or time out while queued
    let permit = match state.scheduler.wait_for_slot(&project_name, rx).await {
        Ok(permit) => permit,
        Err(QueueError::Evicted(_)) => {
            // eviction is already counted in queue.rs try_evict
            let mut resp = error_response(
                StatusCode::TOO_MANY_REQUESTS,
                "evicted",
                format!(
                    "Request evicted due to higher priority traffic (project: {})",
                    project_name
                ),
            );
            resp.headers_mut()
                .insert("retry-after", "5".parse().unwrap());
            resp.headers_mut()
                .insert("x-request-id", request_id.parse().unwrap());
            return resp;
        }
        Err(QueueError::Timeout(_)) => {
            metrics::REQUESTS_TOTAL
                .with_label_values(&[&project_name, "timeout"])
                .inc();

            let mut resp = error_response(
                StatusCode::GATEWAY_TIMEOUT,
                "timeout",
                format!(
                    "Request timed out waiting for a slot (project: {})",
                    project_name
                ),
            );
            resp.headers_mut()
                .insert("x-request-id", request_id.parse().unwrap());
            return resp;
        }
        Err(e) => {
            tracing::error!("Unexpected slot error: {}", e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    tracing::debug!(
        request_id = %request_id,
        project = %project_name,
        queue_ms = request_start.elapsed().as_millis(),
        "slot acquired, forwarding to upstream"
    );

    // Forward to upstream
    let upstream_url = format!("{}/v1/chat/completions", state.config.upstream.url);
    let upstream_start = Instant::now();

    let mut upstream_req = state.http_client.post(&upstream_url).body(body);

    if let Some(auth) = headers.get("authorization") {
        upstream_req = upstream_req.header("authorization", auth);
    } else if let Some(key) = &state.config.upstream.api_key {
        upstream_req = upstream_req.header("authorization", format!("Bearer {}", key));
    }

    for (name, value) in headers.iter() {
        let n = name.as_str();
        if n == "content-type" || n.starts_with("x-") {
            upstream_req = upstream_req.header(name, value);
        }
    }

    let upstream_resp = match upstream_req.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(request_id = %request_id, "upstream request failed: {}", e);
            metrics::REQUESTS_TOTAL
                .with_label_values(&[&project_name, "upstream_error"])
                .inc();
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let status = upstream_resp.status();
    let resp_headers = upstream_resp.headers().clone();

    let is_streaming = resp_headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.contains("text/event-stream"))
        .unwrap_or(false);

    if is_streaming {
        if !status.is_success() {
            metrics::REQUESTS_TOTAL
                .with_label_values(&[&project_name, "upstream_error"])
                .inc();
            return StatusCode::BAD_GATEWAY.into_response();
        }

        let stream = upstream_resp.bytes_stream();
        let sse_stream = streaming_response(
            stream,
            project_name.clone(),
            request_id.clone(),
            request_start,
            upstream_start,
            permit, // permit lives until stream is exhausted or dropped
        );

        let mut resp = Sse::new(sse_stream).into_response();
        resp.headers_mut()
            .insert("x-request-id", request_id.parse().unwrap());
        resp.headers_mut()
            .insert("cache-control", "no-cache".parse().unwrap());
        resp.headers_mut()
            .insert("x-accel-buffering", "no".parse().unwrap());
        resp
    } else {
        let body_bytes = match upstream_resp.bytes().await {
            Ok(b) => b,
            Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
        };
        // Slot is held until here — upstream has finished sending the response body
        drop(permit);

        let upstream_ms = upstream_start.elapsed();
        let total_ms = request_start.elapsed();
        metrics::UPSTREAM_DURATION
            .with_label_values(&[&project_name])
            .observe(upstream_ms.as_secs_f64());
        metrics::REQUESTS_TOTAL
            .with_label_values(&[
                &project_name,
                if status.is_success() {
                    "success"
                } else {
                    "upstream_error"
                },
            ])
            .inc();

        tracing::info!(
            request_id = %request_id,
            project = %project_name,
            status = %status,
            queue_ms = request_start.elapsed().as_millis(),
            upstream_ms = upstream_ms.as_millis(),
            total_ms = total_ms.as_millis(),
            "sync request completed"
        );

        let mut builder = axum::http::Response::builder()
            .status(status)
            .header("x-request-id", &request_id);

        for (name, value) in resp_headers.iter() {
            let n = name.as_str();
            if n == "content-type" || n.starts_with("x-") || n.starts_with("openai-") {
                builder = builder.header(name, value);
            }
        }

        builder.body(axum::body::Body::from(body_bytes)).unwrap()
    }
}

fn streaming_response(
    stream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + Unpin + 'static,
    project: String,
    request_id: String,
    request_start: Instant,
    upstream_start: Instant,
    permit: SlotPermit,
) -> impl Stream<Item = Result<axum::response::sse::Event, Infallible>> {
    let project = Arc::new(project);
    let request_id = Arc::new(request_id);
    let first_token_at: Arc<std::sync::Mutex<Option<Instant>>> =
        Arc::new(std::sync::Mutex::new(None));

    let inner = stream.flat_map(move |chunk_result| {
        let project = Arc::clone(&project);
        let request_id = Arc::clone(&request_id);
        let first_token_at = Arc::clone(&first_token_at);
        let upstream_start = upstream_start;

        let events: Vec<Result<axum::response::sse::Event, Infallible>> = match chunk_result {
            Err(e) => {
                tracing::error!(request_id = %request_id, "upstream stream error: {}", e);
                metrics::UPSTREAM_STREAM_ERRORS.with_label_values(&[&project]).inc();
                metrics::REQUESTS_TOTAL.with_label_values(&[&project, "upstream_error"]).inc();
                vec![]
            }
            Ok(chunk) => {
                let chunk_str = String::from_utf8_lossy(&chunk);
                let mut events = Vec::new();

                for line in chunk_str.lines() {
                    if !line.starts_with("data: ") {
                        continue;
                    }
                    let data = &line[6..];

                    // Record TTFT on first data line
                    {
                        let mut guard = first_token_at.lock().unwrap();
                        if guard.is_none() {
                            *guard = Some(Instant::now());
                            let ttft = request_start.elapsed();
                            metrics::TTFT.with_label_values(&[&project]).observe(ttft.as_secs_f64());
                            tracing::debug!(request_id = %request_id, ttft_ms = ttft.as_millis(), "first token");
                        }
                    }

                    if data == "[DONE]" {
                        let upstream_duration = upstream_start.elapsed();
                        metrics::UPSTREAM_DURATION
                            .with_label_values(&[&project])
                            .observe(upstream_duration.as_secs_f64());

                        let stream_start = first_token_at.lock().unwrap();
                        if let Some(t) = *stream_start {
                            let stream_duration = t.elapsed();
                            metrics::STREAM_DURATION
                                .with_label_values(&[&project])
                                .observe(stream_duration.as_secs_f64());
                            tracing::info!(
                                request_id = %request_id,
                                project = %project,
                                upstream_ms = upstream_duration.as_millis(),
                                stream_ms = stream_duration.as_millis(),
                                "stream completed"
                            );
                        }
                        metrics::REQUESTS_TOTAL.with_label_values(&[&project, "success"]).inc();
                    }

                    events.push(Ok(axum::response::sse::Event::default().data(data)));
                }

                if events.is_empty() {
                    vec![Ok(axum::response::sse::Event::default().comment(""))]
                } else {
                    events
                }
            }
        };

        futures::stream::iter(events)
    });

    PermitStream {
        inner,
        _permit: permit,
    }
}

pub async fn health_check() -> StatusCode {
    StatusCode::OK
}

pub async fn status(State(state): State<AppState>) -> Json<Value> {
    let mut projects = serde_json::Map::new();
    for name in state.config.projects.keys() {
        projects.insert(
            name.clone(),
            serde_json::json!({
                "queue_size": state.scheduler.queue_size(name).await,
                "in_flight": state.scheduler.in_flight_count(name).await,
            }),
        );
    }

    Json(serde_json::json!({
        "status": "ok",
        "slots": state.config.slots,
        "projects": projects,
    }))
}

pub async fn metrics_handler() -> Result<String, StatusCode> {
    metrics::encode_metrics().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
