use lazy_static::lazy_static;
use prometheus::{
    register_counter_vec, register_gauge, register_gauge_vec, register_histogram_vec, CounterVec,
    Encoder, Gauge, GaugeVec, HistogramVec, TextEncoder,
};

lazy_static! {
    // Counter for total requests received
    pub static ref REQUESTS_TOTAL: CounterVec = register_counter_vec!(
        "llm_gateway_requests_total",
        "Total number of requests received",
        &["class", "status"]
    )
    .unwrap();

    // Counter for requests by outcome
    pub static ref REQUESTS_OUTCOME: CounterVec = register_counter_vec!(
        "llm_gateway_requests_outcome_total",
        "Total number of requests by outcome (success, rejected, evicted, timeout)",
        &["outcome"]
    )
    .unwrap();

    // Gauge for queue size by traffic class
    pub static ref QUEUE_SIZE_BY_CLASS: GaugeVec = register_gauge_vec!(
        "llm_gateway_queue_size_by_class",
        "Current number of requests in queue by traffic class",
        &["class"]
    )
    .unwrap();

    // Gauge for in-flight requests by traffic class
    pub static ref IN_FLIGHT_REQUESTS: GaugeVec = register_gauge_vec!(
        "llm_gateway_in_flight_requests",
        "Current number of in-flight requests by traffic class",
        &["class"]
    )
    .unwrap();

    // Gauge for available global processing slots
    pub static ref AVAILABLE_PERMITS: Gauge = register_gauge!(
        "llm_gateway_available_permits",
        "Number of available concurrent processing slots (global)"
    )
    .unwrap();

    // Histogram for queue wait time by class
    pub static ref QUEUE_DURATION: HistogramVec = register_histogram_vec!(
        "llm_gateway_queue_duration_seconds",
        "Time spent waiting in queue by traffic class",
        &["class"],
        vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]
    )
    .unwrap();

    // Histogram for OpenAI API call duration by class
    pub static ref OPENAI_DURATION: HistogramVec = register_histogram_vec!(
        "llm_gateway_upstream_duration_seconds",
        "Time spent calling OpenAI API by traffic class",
        &["class", "status"],
        vec![0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0]
    )
    .unwrap();

    // Counter for upstream OpenAI responses by class and HTTP status code
    pub static ref OPENAI_RESPONSES: CounterVec = register_counter_vec!(
        "llm_gateway_upstream_responses_total",
        "Total number of responses received from OpenAI by traffic class and HTTP status code",
        &["class", "status_code"]
    )
    .unwrap();

    // Histogram for total request duration by class
    pub static ref REQUEST_DURATION: HistogramVec = register_histogram_vec!(
        "llm_gateway_request_duration_seconds",
        "Total request duration including queue and upstream time",
        &["class", "status"],
        vec![0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0]
    )
    .unwrap();

    // Counter for evicted requests by class
    pub static ref REQUESTS_EVICTED: CounterVec = register_counter_vec!(
        "llm_gateway_requests_evicted_total",
        "Total number of requests evicted from queue by traffic class",
        &["class"]
    )
    .unwrap();

    // Counter for rejected requests by class and reason
    pub static ref REQUESTS_REJECTED: CounterVec = register_counter_vec!(
        "llm_gateway_requests_rejected_total",
        "Total number of requests rejected by traffic class and reason",
        &["class", "reason"]
    )
    .unwrap();

    // Counter for unknown/unmapped credentials (aggregate only)
    pub static ref UNKNOWN_CREDENTIALS: CounterVec = register_counter_vec!(
        "llm_gateway_unknown_credentials_total",
        "Total number of requests with unknown or unmapped credentials",
        &["action"]
    )
    .unwrap();

    // Counter for client disconnects
    pub static ref CLIENT_DISCONNECTS: CounterVec = register_counter_vec!(
        "llm_gateway_client_disconnects_total",
        "Total number of client disconnects by traffic class",
        &["class"]
    )
    .unwrap();
}

/// Encode and return metrics in Prometheus text format
pub fn encode_metrics() -> Result<String, prometheus::Error> {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    encoder.encode(&metric_families, &mut buffer)?;
    Ok(String::from_utf8(buffer).unwrap())
}
