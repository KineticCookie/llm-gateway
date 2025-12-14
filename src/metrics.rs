use lazy_static::lazy_static;
use prometheus::{
    register_counter_vec, register_gauge, register_gauge_vec, register_histogram_vec, CounterVec, Encoder, Gauge,
    GaugeVec, HistogramVec, TextEncoder,
};

lazy_static! {
    // Counter for total requests received
    pub static ref REQUESTS_TOTAL: CounterVec = register_counter_vec!(
        "openai_proxy_requests_total",
        "Total number of requests received",
        &["priority", "status"]
    )
    .unwrap();

    // Counter for requests by outcome
    pub static ref REQUESTS_OUTCOME: CounterVec = register_counter_vec!(
        "openai_proxy_requests_outcome_total",
        "Total number of requests by outcome",
        &["outcome"]
    )
    .unwrap();

    // Gauge for current queue size
    pub static ref QUEUE_SIZE: Gauge = register_gauge!(
        "openai_proxy_queue_size",
        "Current number of requests in the queue"
    )
    .unwrap();

    // Gauge for queue size by priority
    pub static ref QUEUE_SIZE_BY_PRIORITY: GaugeVec = register_gauge_vec!(
        "openai_proxy_queue_size_by_priority",
        "Current number of requests in queue by priority bucket",
        &["priority"]
    )
    .unwrap();

    // Gauge for available semaphore permits
    pub static ref AVAILABLE_PERMITS: Gauge = register_gauge!(
        "openai_proxy_available_permits",
        "Number of available concurrent processing slots"
    )
    .unwrap();

    // Histogram for queue wait time
    pub static ref QUEUE_DURATION: HistogramVec = register_histogram_vec!(
        "openai_proxy_queue_duration_seconds",
        "Time spent waiting in queue",
        &["priority"],
        vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]
    )
    .unwrap();

    // Histogram for OpenAI API call duration
    pub static ref OPENAI_DURATION: HistogramVec = register_histogram_vec!(
        "openai_proxy_openai_duration_seconds",
        "Time spent calling OpenAI API",
        &["status"],
        vec![0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0]
    )
    .unwrap();

    // Histogram for total request duration
    pub static ref REQUEST_DURATION: HistogramVec = register_histogram_vec!(
        "openai_proxy_request_duration_seconds",
        "Total request duration",
        &["priority", "status"],
        vec![0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0]
    )
    .unwrap();

    // Counter for evicted requests
    pub static ref REQUESTS_EVICTED: CounterVec = register_counter_vec!(
        "openai_proxy_requests_evicted_total",
        "Total number of requests evicted from queue",
        &["priority"]
    )
    .unwrap();

    // Counter for rejected requests
    pub static ref REQUESTS_REJECTED: CounterVec = register_counter_vec!(
        "openai_proxy_requests_rejected_total",
        "Total number of requests rejected due to low priority",
        &["priority"]
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
