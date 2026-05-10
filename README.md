# LLM Gateway

A lightweight gateway for OpenAI-compatible APIs that helps multiple teams share limited GPU / LLM capacity without stepping on each other. Think of it as a fair queue in front of your self-hosted model servers.

## Why I built this

I built this after seeing the same thing happen over and over with shared on-prem LLM servers: one team starts a heavy workload, the GPUs get saturated, and everyone else's requests — often live demos — just hang.

The usual advice is "just add more GPUs", but that's expensive and doesn't really fix the problem. Without explicit rules, shared GPU capacity turns into a free-for-all: some workloads dominate, others starve, and things still break in unpredictable ways.

This gateway makes those rules explicit. Instead of teams competing implicitly for GPU time, you define clear projects with priority tiers and slot limits, and the gateway enforces them.

The end result is much more boring — in a good way. Demos stay responsive, background jobs keep moving, and the system degrades gracefully instead of falling over all at once.

## What It Does

The gateway sits between your clients and an OpenAI-compatible endpoint (e.g. llama.cpp server) and controls how many requests each project can run at the same time. It handles queuing, fairness, and backpressure so one team can't accidentally saturate everything.

- Define named projects (e.g. `production`, `batch`, `team-alpha`)
- Assign each project a priority tier — lower number = higher importance
- Within a tier, share capacity by configurable weight
- Queue requests when capacity is full, with per-project timeouts
- Evict lower-priority queued requests when higher-priority traffic needs room
- Drop in front of existing OpenAI-style clients without changing their code
- Full streaming support — slots are held until the last token is sent

## Configuration

Copy `config.example.yaml` to `config.yaml`:

```yaml
server:
  host: "0.0.0.0"
  port: 12345

upstream:
  # Base URL of your llama.cpp or OpenAI-compatible server (no /v1 suffix)
  url: "http://localhost:8080"
  # Optional: API key sent to upstream. If unset, the client's key is forwarded.
  api_key: null

# Total number of llama.cpp inference slots (--parallel N in llama-server)
slots: 8

# Default request timeout (queue wait + inference). Supports: 30s, 5m, 1h
default_timeout: 5m

# What to do with requests that have no matching API key.
# "reject" returns 403. Or set to a project name to route them there.
unauthenticated: reject

projects:
  # Tier 1 — highest importance, always served first
  production:
    priority: 1
    share: 1        # only project at this tier, share is irrelevant
    max_slots: 8    # hard cap on concurrent requests (defaults to global slots)
    timeout: 30s    # tight timeout for live traffic
    api_keys:
      - "sk-prod-xxx"

  # Tier 2 — only runs when tier 1 queue is empty
  staging:
    priority: 2
    share: 2        # gets 2/3 of available capacity vs team-beta's 1/3
    max_slots: 4
    timeout: 2m
    api_keys:
      - "sk-staging-xxx"

  team-beta:
    priority: 2
    share: 1
    max_slots: 4
    timeout: 2m
    api_keys:
      - "sk-beta-xxx"

  # Tier 3 — background/batch work, runs when nothing else is waiting
  batch:
    priority: 3
    max_slots: 2
    timeout: 10m
    api_keys:
      - "sk-batch-xxx"
```

### Key concepts

**`priority`** — which tier a project belongs to. Tier 1 is always dispatched before tier 2, tier 2 before tier 3, etc. Strict ordering across tiers.

**`share`** — relative weight within the same tier. If two projects share tier 2 with shares 2 and 1, they get roughly 67% and 33% of available capacity respectively. Defaults to 1.

**`max_slots`** — hard cap on how many requests this project can have in-flight simultaneously. Defaults to the global `slots` value.

**`timeout`** — maximum time a request can spend waiting in queue + being served. Overrides `default_timeout` per project.

## How Scheduling Works

Each project has its own FIFO queue. A background dispatcher loop wakes up whenever a slot is freed or a new request arrives:

1. Groups all projects with queued work by priority tier
2. Takes the lowest tier number (highest importance)
3. Among projects in that tier, picks one weighted by `share`
4. Dispatches one request from that project's queue

Eviction happens when the system is fully saturated and a higher-importance project has work waiting. The lowest-importance project with a queued request has its newest entry evicted (429 Too Many Requests). Running requests are never interrupted.

## Running

```bash
# Directly
cargo run --release

# With Docker Compose (includes Prometheus + Grafana + Loki)
docker-compose up -d
```

## Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/v1/chat/completions` | Proxied + scheduled (sync and streaming) |
| `GET` | `/health` | Returns 200 OK |
| `GET` | `/status` | JSON with per-project queue depth and in-flight count |
| `GET` | `/metrics` | Prometheus metrics |

## Error Responses

All errors use OpenAI-style JSON bodies (`{"error": {"type": "...", "message": "..."}}`):

| Status | Type | Meaning |
|--------|------|---------|
| 403 | `no_credentials` / `unknown_key` | No API key or key not found in any project |
| 503 | `queue_full` | Project queue is full, request rejected immediately |
| 429 | `evicted` | Request was queued but evicted by higher-priority traffic |
| 504 | `timeout` | Request timed out waiting for a slot |
| 502 | — | Upstream returned an error |

Retryable responses include a `Retry-After: 5` header and an `x-request-id` for tracing.

## Observability

Prometheus metrics at `/metrics`, with a pre-built Grafana dashboard in `observability/`.

Key metrics:

```
llm_gateway_slots_in_use                         # current global in-flight count
llm_gateway_slots_total                          # total configured slots
llm_gateway_project_in_flight{project}           # per-project in-flight
llm_gateway_project_queue_depth{project}         # per-project queue depth
llm_gateway_requests_total{project, outcome}     # outcome: success|rejected|evicted|timeout|upstream_error
llm_gateway_queue_wait_seconds{project}          # histogram: time waiting for a slot
llm_gateway_upstream_duration_seconds{project}   # histogram: time from slot acquired to response done
llm_gateway_ttft_seconds{project}               # histogram: streaming time-to-first-token
llm_gateway_stream_duration_seconds{project}    # histogram: streaming first→last token duration
```

Start the full observability stack:

```bash
docker-compose up -d
# Grafana at http://localhost:3000 (admin/admin)
# Prometheus at http://localhost:9090
```

## Limitations

- Only `/v1/chat/completions` is scheduled; all other endpoints are not proxied
- Eviction removes queued requests only — running requests are never interrupted
- No per-user rate limiting, only per-project
