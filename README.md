# LLM Gateway

A lightweight gateway for OpenAI-compatible APIs that helps multiple teams share limited GPU / LLM capacity without stepping on each other. Think of it as a fair queue in front of your self-hosted model servers.

## Why I built this

I built this after seeing the same thing happen over and over again with shared on-prem LLM servers: one team starts a heavy workload, the GPUs get saturated, and everyone else’s requests — often live demos — just hang.

The usual advice is “just add more GPUs”, but that’s expensive and doesn’t really fix the problem. Without explicit rules, shared GPU capacity turns into a free-for-all: some workloads dominate, others starve, and things still break in unpredictable ways.

This gateway makes those rules explicit. Instead of teams competing implicitly for GPU time, you define clear traffic classes with guarantees and limits, and the gateway enforces them.

The end result is much more boring — in a good way. Demos stay responsive, background jobs keep moving, and the system degrades gracefully instead of falling over all at once.

## What It Does

The gateway sits between your clients and an OpenAI-compatible endpoint and controls how many requests each group can run at the same time. It handles queuing, fairness, and backpressure so one team can’t accidentally take everything down.

At a high level, it lets you:

- Define traffic classes (for example: `production`, `staging`, or `team-alpha`)
- Give each class minimum and maximum concurrency guarantees
- Queue requests when capacity is full instead of timing out immediately
- Share throughput fairly using simple, configurable weights
- Kick out lower-priority queued requests when higher-priority traffic needs room
- Drop it in front of existing OpenAI-style clients without changing their code

## How It Works

### Traffic Classes

You define a small set of named traffic classes and map each API key to exactly one of them. A class can represent a team, an environment, or just a priority level — whatever makes sense for your setup.

Each class has a few knobs:

- **weight** — how much throughput it should get relative to others
- **priority** — how protected it is from eviction under load
- **min_concurrency** — how many concurrent requests it’s guaranteed when it has work
- **max_concurrency** — a hard cap on how many requests it can run at once
- **max_queue_size** — how many requests can wait before new ones get rejected

Example:

```yaml
classes:
  production:
    weight: 6
    priority: 100
    min_concurrency: 3
    max_concurrency: 8

  staging:
    weight: 3
    priority: 50
    min_concurrency: 1
    max_concurrency: 5

  testing:
    weight: 1
    priority: 10
    min_concurrency: 0
    max_concurrency: 3
```

In short:
weights decide who gets more throughput, priorities decide who gets kicked out last.

### Scheduling Logic

Internally, each class has its own FIFO queue.

When a request comes in:

The gateway reads the API key and maps it to a traffic class

If that class’s queue is already full, the request is rejected

Otherwise, it gets queued and waits for capacity

Meanwhile, a dispatcher loop continuously looks for work to run:

If there’s no global capacity left, it checks whether eviction is needed

It looks at classes that still have room under their max_concurrency

Classes that haven’t reached their min_concurrency get picked first

Among the remaining candidates, it picks a class based on weight

One request from that class is dispatched

The result is simple and predictable: every class gets its minimum first, and any leftover capacity is shared according to weights.

### Eviction (when things get tight)

Eviction only happens when the system is fully saturated and a higher-priority class still hasn’t reached its guaranteed minimum.

In that case, the gateway:

1. Finds the lowest-priority class with queued requests

2. Removes the newest request from that queue

3. Immediately responds with a 429 Too Many Requests and a Retry-After header

Running requests are never interrupted — eviction only affects queued work.

### Authentication & Trust Model

The gateway doesn’t try to be clever about auth. You configure it.

API keys are mapped to traffic classes in the config file:

```yaml
credentials:
  api_keys:
    "sk-prod-xxx": "production"
    "sk-staging-xxx": "staging"
  default_class: null
  fallback_class: null
```

Clients send normal OpenAI-style requests. They don’t get to pick their own priority.

## Configuration

Copy config.example.yaml to config.yaml and adjust it for your setup. Most of the knobs are about how you want to divide limited capacity between groups.

> [!NOTE]
> The sum of min_concurrency across all classes must be less than or equal to global_concurrency.
>
>If you break this rule, some guarantees can’t be satisfied.

## Error Handling

Errors are returned using OpenAI-style JSON responses:

- 403 — unknown API key or broken class mapping
- 503 (queue_full) — this class’s queue is full
- 429 (evicted) — the request was queued but pushed out by higher-priority traffic
- 504 (timeout) — the request spent too long in the system overall

Retryable errors include a Retry-After header and a request ID for debugging.

## Observability

The gateway exposes Prometheus metrics at /metrics and logs all scheduling decisions.

You can see:

- how many requests are queued or running per class
- who’s getting evicted or rejected
- how long requests spend waiting vs. running

There’s also a prebuilt Grafana dashboard if you want something visual.

## Limitations & Gotchas

This gateway:

- schedules and queues /v1/chat/completions
- helps multiple teams share a single pool of GPUs sanely

It does not:

- stream responses (yet)
- handle every OpenAI endpoint
- guarantee perfect fairness under all conditions
- preempt running requests

Use it as a building block, not a complete solution.
