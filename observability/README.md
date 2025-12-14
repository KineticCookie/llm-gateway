# Observability Stack

This directory contains the configuration for the observability stack (Prometheus, Grafana, Loki, Promtail) that monitors the OpenAI API Proxy.

## Architecture

- **Prometheus**: Metrics collection and storage
- **Grafana**: Visualization and dashboards
- **Loki**: Log aggregation
- **Promtail**: Log shipping

The proxy runs on the host machine, while the observability stack runs in Docker containers.

## Quick Start

1. **Start the observability stack:**
   ```bash
   docker-compose up -d
   ```

2. **Run the proxy on your host machine:**
   ```bash
   # With JSON logging for Loki (recommended)
   RUST_LOG=info cargo run --release 2>&1 | tee -a logs/proxy.log
   
   # Or with default logging
   cargo run --release
   ```

3. **Access the dashboards:**
   - Grafana: http://localhost:3000 (admin/admin)
   - Prometheus: http://localhost:9090
   - Loki: http://localhost:3100

## Grafana Dashboard

The pre-configured dashboard includes:

### Metrics Panels:
- **Current Queue Size**: Real-time gauge of queued requests
- **Available Processing Slots**: Concurrent request capacity
- **Request Rate by Priority**: Requests per second by priority level
- **Queue Wait Time**: p50, p95, p99 percentiles of time spent in queue
- **OpenAI API Latency**: p50, p95, p99 percentiles of upstream API calls
- **Request Outcomes**: Success, evicted, and rejected request rates
- **Evictions (Last Hour)**: Bar chart of evictions by priority
- **Rejections (Last Hour)**: Bar chart of rejections by priority
- **Proxy Logs**: Live log streaming from Loki

## Accessing Metrics

The proxy exposes Prometheus metrics at: http://localhost:8080/metrics

Example metrics:
```
# Queue metrics
openai_proxy_queue_size
openai_proxy_available_permits

# Request counters
openai_proxy_requests_total{priority_level="normal",status="completed"}
openai_proxy_requests_outcome_total{outcome="success"}
openai_proxy_requests_evicted_total{evicted_priority="low"}
openai_proxy_requests_rejected_total{priority_level="very_low"}

# Latency histograms
openai_proxy_queue_duration_seconds
openai_proxy_openai_duration_seconds
openai_proxy_request_duration_seconds
```

## Log Collection

Promtail collects logs from:
- `./logs/*.log` - Proxy logs (if you redirect output)

To enable log collection, run the proxy with output redirection:
```bash
mkdir -p logs
cargo run --release 2>&1 | tee -a logs/proxy.log
```

## Querying Logs in Grafana

Example LogQL queries:
```
{job="openai-proxy"}
{job="openai-proxy"} |= "error"
{job="openai-proxy"} |= "evicted"
{job="openai-proxy"} | json | priority > 50
```

## Data Retention

- **Prometheus**: 7 days (configurable in prometheus.yml)
- **Loki**: 7 days (configurable in loki-config.yml)

## Stopping the Stack

```bash
docker-compose down

# To also remove volumes (data will be lost)
docker-compose down -v
```

## Customization

### Adding More Metrics

Edit `prometheus.yml` to add more scrape targets:
```yaml
scrape_configs:
  - job_name: 'my-service'
    static_configs:
      - targets: ['host.docker.internal:9091']
```

### Modifying Dashboards

1. Edit dashboards in Grafana UI
2. Export JSON via Share > Export
3. Save to `grafana/dashboards/` directory
4. Restart Grafana container

## Troubleshooting

### Prometheus can't reach the proxy

Ensure the proxy is running on port 8080 and check:
```bash
curl http://localhost:8080/metrics
```

### Logs not appearing in Grafana

1. Check Promtail is running: `docker-compose ps`
2. Verify log files exist: `ls -la logs/`
3. Check Promtail logs: `docker-compose logs promtail`

### Dashboard not loading

1. Restart Grafana: `docker-compose restart grafana`
2. Check provisioning logs: `docker-compose logs grafana`
