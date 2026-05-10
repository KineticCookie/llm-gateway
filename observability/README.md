# Observability Stack

This directory contains the configuration for the observability stack (Prometheus, Grafana, Loki, Promtail) that monitors the LLM Gateway.

## Architecture

- **Prometheus**: Metrics collection and storage
- **Grafana**: Visualization and dashboards
- **Loki**: Log aggregation
- **Promtail**: Log shipping

All services run in Docker containers on the same `observability` network. The proxy container is named `proxy` and is scraped directly by Prometheus.

## Quick Start

1. **Start the full stack (proxy + observability):**
   ```bash
   docker-compose up -d
   ```

2. **Access the dashboards:**
   - Grafana: http://localhost:3000 (admin/admin)
   - Prometheus: http://localhost:9090
   - Loki: http://localhost:3100

3. **Check proxy metrics directly:**
   ```bash
   curl http://localhost:12345/metrics
   ```

## Grafana Dashboard

The pre-configured dashboard includes:

### Capacity Panels
- **Slot Utilization**: Gauge showing `slots_in_use / slots_total` as a percentage
- **Queued Requests**: Total requests waiting across all projects
- **Slot Capacity**: Time series of in-use vs total slots with threshold line
- **In-Flight Requests by Project**: Per-project concurrent request count

### Traffic Panels
- **Successful Request Rate by Project**: req/s broken down by project
- **Request Outcomes**: Success, evicted, rejected, timeout, upstream_error rates
- **Queue Depth by Project**: Stacked queue depth over time per project

### Latency Panels
- **Queue Wait Time**: p50/p95/p99 time spent waiting for a slot
- **Upstream Duration**: p50/p95/p99 time from slot acquired to response done (sync: full body, streaming: `[DONE]`)
- **STREAMING: Time To First Token (TTFT)**: p50/p95/p99 per project (includes queue wait)
- **STREAMING: Stream Duration**: p50/p95/p99 time from first to last token per project

### Error Panels
- **Upstream Stream Errors**: Mid-stream read failures from upstream
- **Unknown API Key Requests**: Requests with missing or unrecognized keys (by reason)

### Logs
- **Proxy Logs**: Live log streaming from Loki

## Metrics Reference

```
# Capacity
llm_gateway_slots_total                          # static — total slots from config
llm_gateway_slots_in_use                         # current global in-flight count
llm_gateway_project_in_flight{project}           # per-project in-flight
llm_gateway_project_queue_depth{project}         # per-project queue depth

# Throughput
llm_gateway_requests_total{project, outcome}
# outcomes: success | rejected | evicted | timeout | upstream_error

# Latency (histograms)
llm_gateway_queue_wait_seconds{project}          # time waiting in queue
llm_gateway_upstream_duration_seconds{project}   # slot acquired → response done
llm_gateway_ttft_seconds{project}               # streaming: request → first token
llm_gateway_stream_duration_seconds{project}    # streaming: first token → last token

# Errors
llm_gateway_unknown_credentials_total{reason}   # reason: no_key | unknown_key
llm_gateway_upstream_stream_errors_total{project}
```

## Log Collection

Promtail collects logs from the proxy container via the Docker socket. Logs appear automatically in Grafana under the `{job="llm-gateway"}` label.

Example LogQL queries:
```
{job="llm-gateway"}
{job="llm-gateway"} |= "error"
{job="llm-gateway"} |= "evicted"
{job="llm-gateway"} |= "stream completed"
```

## Data Retention

- **Prometheus**: 7 days
- **Loki**: 7 days

## Stopping the Stack

```bash
docker-compose down

# To also remove volumes (data will be lost)
docker-compose down -v
```

## Troubleshooting

### Prometheus can't reach the proxy

Ensure the proxy container is running and on the `observability` network:
```bash
docker-compose ps
curl http://localhost:12345/metrics
```

### Logs not appearing in Grafana

1. Check Promtail is running: `docker-compose ps`
2. Check Promtail logs: `docker-compose logs promtail`
3. Verify the proxy container name matches the Promtail config (`llm-gateway`)

### Dashboard not loading

1. Restart Grafana: `docker-compose restart grafana`
2. Check provisioning logs: `docker-compose logs grafana`
