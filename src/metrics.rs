use lazy_static::lazy_static;
use prometheus::{
    register_counter_vec, register_gauge, register_gauge_vec, register_histogram_vec, CounterVec,
    Encoder, Gauge, GaugeVec, HistogramVec, TextEncoder,
};

lazy_static! {
    // ------------------------------------------------------------------
    // Capacity — current state of the slot pool
    // ------------------------------------------------------------------

    /// Static total from config — useful as denominator for utilization.
    pub static ref SLOTS_TOTAL: Gauge = register_gauge!(
        "llm_gateway_slots_total",
        "Total number of inference slots configured (llama.cpp --parallel)"
    ).unwrap();

    /// How many slots are currently occupied (global).
    pub static ref SLOTS_IN_USE: Gauge = register_gauge!(
        "llm_gateway_slots_in_use",
        "Number of inference slots currently in use"
    ).unwrap();

    /// Per-project in-flight count.
    pub static ref PROJECT_IN_FLIGHT: GaugeVec = register_gauge_vec!(
        "llm_gateway_project_in_flight",
        "Number of requests currently occupying a slot per project",
        &["project"]
    ).unwrap();

    /// Per-project queue depth.
    pub static ref PROJECT_QUEUE_DEPTH: GaugeVec = register_gauge_vec!(
        "llm_gateway_project_queue_depth",
        "Number of requests waiting in queue per project",
        &["project"]
    ).unwrap();

    // ------------------------------------------------------------------
    // Throughput — what happened to each request
    // outcome: success | rejected | evicted | timeout | upstream_error
    // ------------------------------------------------------------------

    pub static ref REQUESTS_TOTAL: CounterVec = register_counter_vec!(
        "llm_gateway_requests_total",
        "Total requests by project and outcome",
        &["project", "outcome"]
    ).unwrap();

    // ------------------------------------------------------------------
    // Latency
    // ------------------------------------------------------------------

    /// Time a request spent waiting in queue before a slot was granted.
    /// Recorded on dispatch and eviction. Timeout items are recorded when
    /// the dispatch loop finds the closed receiver and discards the item.
    pub static ref QUEUE_WAIT: HistogramVec = register_histogram_vec!(
        "llm_gateway_queue_wait_seconds",
        "Time spent waiting in queue before a slot was granted or the request was evicted",
        &["project"],
        vec![0.001, 0.005, 0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0]
    ).unwrap();

    /// Time from slot acquired to response fully received from upstream.
    /// For sync: time to full response body.
    /// For streaming: time from first byte to [DONE].
    pub static ref UPSTREAM_DURATION: HistogramVec = register_histogram_vec!(
        "llm_gateway_upstream_duration_seconds",
        "Time from slot acquired to upstream response complete (sync: full body, streaming: [DONE])",
        &["project"],
        vec![0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0]
    ).unwrap();

    /// Streaming only: time from request received to first token (includes queue wait).
    pub static ref TTFT: HistogramVec = register_histogram_vec!(
        "llm_gateway_ttft_seconds",
        "Time to first token for streaming requests (queue wait + upstream TTFT)",
        &["project"],
        vec![0.1, 0.25, 0.5, 1.0, 2.0, 3.0, 5.0, 10.0, 15.0, 30.0]
    ).unwrap();

    /// Streaming only: duration from first token to last token.
    pub static ref STREAM_DURATION: HistogramVec = register_histogram_vec!(
        "llm_gateway_stream_duration_seconds",
        "Streaming duration from first token to last token",
        &["project"],
        vec![1.0, 5.0, 10.0, 20.0, 30.0, 45.0, 60.0, 90.0, 120.0, 180.0]
    ).unwrap();

    // ------------------------------------------------------------------
    // Errors
    // ------------------------------------------------------------------

    /// Requests with no matching API key (no_key | unknown_key).
    pub static ref UNKNOWN_CREDENTIALS: CounterVec = register_counter_vec!(
        "llm_gateway_unknown_credentials_total",
        "Requests with missing or unknown API key",
        &["reason"]
    ).unwrap();

    /// Errors reading from the upstream stream mid-response.
    pub static ref UPSTREAM_STREAM_ERRORS: CounterVec = register_counter_vec!(
        "llm_gateway_upstream_stream_errors_total",
        "Errors reading from upstream during streaming",
        &["project"]
    ).unwrap();
}

/// Pre-initialize all per-project label combinations so they appear in
/// /metrics from startup, even before any requests arrive.
pub fn init_project_metrics(project_names: impl Iterator<Item = impl AsRef<str>>) {
    for name in project_names {
        let n = name.as_ref();
        PROJECT_IN_FLIGHT.with_label_values(&[n]).set(0.0);
        PROJECT_QUEUE_DEPTH.with_label_values(&[n]).set(0.0);
        // Touch counters and histograms so they show up with zero values
        REQUESTS_TOTAL.with_label_values(&[n, "success"]).reset();
        REQUESTS_TOTAL.with_label_values(&[n, "rejected"]).reset();
        REQUESTS_TOTAL.with_label_values(&[n, "evicted"]).reset();
        REQUESTS_TOTAL.with_label_values(&[n, "timeout"]).reset();
        REQUESTS_TOTAL.with_label_values(&[n, "upstream_error"]).reset();
        UPSTREAM_STREAM_ERRORS.with_label_values(&[n]).reset();
    }
}

pub fn encode_metrics() -> Result<String, prometheus::Error> {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    encoder.encode(&metric_families, &mut buffer)?;
    Ok(String::from_utf8(buffer).unwrap())
}
