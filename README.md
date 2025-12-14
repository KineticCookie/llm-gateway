# OpenAI API Proxy

A thin, high-performance proxy for the OpenAI API with priority-based request queuing.

## Features

- **Priority Queue**: Request prioritization with automatic eviction of lower-priority requests when queue is full
- **OpenAI API Compatible**: Mirrors the OpenAI API endpoints with minimal extensions
- **Configuration-based**: Control plane managed via YAML config files
- **Concurrent Processing**: Configurable concurrency limits for optimal resource utilization

## Architecture

### Data Plane

The proxy mirrors the OpenAI API with one extension:
- **POST /v1/chat/completions**: Chat completions endpoint with optional `priority` parameter
  - Add `"priority": <integer>` to the request body to set priority (default: 0)
  - Higher values = higher priority
  - If queue is full and incoming request priority >= lowest queued priority, the lowest priority request is evicted

### Control Plane

Configuration is managed via `config.yaml`:
- **Server settings**: Host and port configuration
- **Upstream settings**: OpenAI API URL and optional API key
- **Queue settings**: Max queue size and timeout settings

## Configuration

Create a `config.yaml` file:

```yaml
server:
  host: "0.0.0.0"
  port: 8080

upstream:
  openai_api_url: "https://api.openai.com"
  api_key: null  # Optional: set your API key or pass via Authorization header

queue:
  max_queue_size: 100
  timeout_seconds: 300
  concurrent_limit: 10
```

You can also use environment variables with the `PROXY_` prefix:
- `PROXY_SERVER__HOST`
- `PROXY_SERVER__PORT`
- `PROXY_UPSTREAM__OPENAI_API_URL`
- `PROXY_UPSTREAM__API_KEY`
- `PROXY_QUEUE__MAX_QUEUE_SIZE`
- `PROXY_QUEUE__TIMEOUT_SECONDS`
- `PROXY_QUEUE__CONCURRENT_LIMIT`

## Usage

### Building

```bash
cargo build --release
```

### Running

```bash
cargo run --release
```

Or with the binary:
```bash
./target/release/openai-api-proxy
```

### Making Requests

Standard OpenAI API request (priority 0):
```bash
curl -X POST http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer YOUR_API_KEY" \
  -d '{
    "model": "gpt-4",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

High priority request:
```bash
curl -X POST http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer YOUR_API_KEY" \
  -d '{
    "model": "gpt-4",
    "messages": [{"role": "user", "content": "Urgent request!"}],
    "priority": 100
  }'
```

Health check:
```bash
curl http://localhost:8080/health
```

## Priority Queue Behavior

1. Requests are queued based on priority (higher values = higher priority)
2. Within the same priority level, requests are processed FIFO
3. When queue reaches `max_queue_size`:
   - If new request priority >= lowest queued priority: evict lowest and queue new request
   - If new request priority < lowest queued priority: reject with 503 error
4. Evicted requests receive a 503 error with type "evicted"
5. Concurrent processing limit controlled by `concurrent_limit` config (default: 10)

## Error Responses

The proxy returns standard OpenAI-compatible error responses:

- **503 Service Unavailable** (queue_full): Queue is full and request priority too low
- **503 Service Unavailable** (evicted): Request was evicted by higher priority request
- **504 Gateway Timeout** (timeout): Request exceeded timeout limit

## Observability & Monitoring

The proxy exposes Prometheus metrics at `/metrics` endpoint and includes a complete observability stack.

### Quick Start with Observability

Run the proxy with the observability stack (Prometheus, Grafana, Loki):

```bash
./bin/start-demo.sh
```

This starts:
- **Grafana** at http://localhost:3000 (admin/admin) - Pre-configured dashboards
- **Prometheus** at http://localhost:9090 - Metrics storage
- **Loki** at http://localhost:3100 - Log aggregation

### Metrics Exposed

- `openai_proxy_queue_size` - Current queue size
- `openai_proxy_available_permits` - Available processing slots
- `openai_proxy_requests_total` - Total requests by priority and status
- `openai_proxy_requests_outcome_total` - Requests by outcome (success/evicted/rejected)
- `openai_proxy_queue_duration_seconds` - Queue wait time histogram
- `openai_proxy_openai_duration_seconds` - OpenAI API call duration histogram
- `openai_proxy_request_duration_seconds` - Total request duration histogram
- `openai_proxy_requests_evicted_total` - Evicted requests counter
- `openai_proxy_requests_rejected_total` - Rejected requests counter

### Grafana Dashboard

The included dashboard shows:
- Real-time queue size and available slots
- Request rate by priority level
- Latency percentiles (p50, p95, p99) for queue time and OpenAI API calls
- Request outcomes (success/evicted/rejected)
- Evictions and rejections by priority
- Live log streaming

See [observability/README.md](observability/README.md) for detailed documentation.

## Logging

The proxy includes comprehensive logging for queue operations:

- **INFO level**: Request lifecycle (enqueued, dequeued, completed), evictions, latency metrics
- **DEBUG level**: Queue state, semaphore permits, request forwarding
- **WARN level**: Queue full conditions, rejections, evictions
- **ERROR level**: Upstream failures

Run with different log levels:
```bash
# Info level (default)
cargo run --release

# Debug level (detailed queue operations)
RUST_LOG=debug cargo run

# Trace level (maximum verbosity)
RUST_LOG=trace cargo run

# Component-specific logging
RUST_LOG=openai_api_proxy::queue=debug cargo run
```

## Development

Run with debug logging:
```bash
RUST_LOG=debug cargo run
```

Run tests:
```bash
cargo test
```

## License

MIT
