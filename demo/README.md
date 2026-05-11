# Demo

Self-contained stack: llama.cpp + LLM Gateway + Prometheus + Grafana + Loki.

## What's included

| Service | URL | Description |
|---------|-----|-------------|
| LLM Gateway | http://localhost:12345 | Proxy (OpenAI-compatible) |
| Grafana | http://localhost:3000 | Dashboards (admin/admin) |
| Prometheus | http://localhost:9090 | Metrics |

## Start

```bash
cd demo/
docker compose up -d
```

On first start, llama.cpp downloads `unsloth/gemma-3-1b-it-GGUF` (~1GB). The proxy won't start until the model is loaded. Check progress:

```bash
docker compose logs -f llama
```

## Try it

```bash
# Critical priority
curl http://localhost:12345/v1/chat/completions \
  -H "Authorization: Bearer demo-key-critical" \
  -H "Content-Type: application/json" \
  -d '{"model":"gemma","messages":[{"role":"user","content":"Hello!"}],"stream":false}'

# Streaming
curl http://localhost:12345/v1/chat/completions \
  -H "Authorization: Bearer demo-key-normal" \
  -H "Content-Type: application/json" \
  -d '{"model":"gemma","messages":[{"role":"user","content":"Count to 5"}],"stream":true}'
```

API keys by project:

| Key | Project | Priority |
|-----|---------|----------|
| `demo-key-critical` | critical | 1 (highest) |
| `demo-key-high` | high | 2 |
| `demo-key-normal` | normal | 2 |
| `demo-key-low` | low | 3 (lowest) |

## Stop

```bash
docker compose down

# Also remove model cache and data volumes
docker compose down -v
```
