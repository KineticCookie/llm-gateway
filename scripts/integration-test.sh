#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

UPSTREAM_URL="http://localhost:8080"
COMPOSE_FILE="docker-compose.test.yml"

cleanup() {
    echo "Tearing down llama.cpp test server..."
    docker compose -f "$COMPOSE_FILE" down
}
trap cleanup EXIT

echo "Starting llama.cpp test server..."
docker compose -f "$COMPOSE_FILE" up -d

echo "Waiting for llama.cpp to be ready (model download + load may take a while)..."
for i in $(seq 1 200); do
    if curl -sf "$UPSTREAM_URL/health" > /dev/null 2>&1; then
        echo "llama.cpp server is ready"
        break
    fi
    if [ "$i" -eq 200 ]; then
        echo "Timed out waiting for llama.cpp server" >&2
        docker compose -f "$COMPOSE_FILE" logs llama-server >&2
        exit 1
    fi
    echo "  waiting... ($i/200)"
    sleep 3
done

echo ""
echo "Running integration tests against $UPSTREAM_URL ..."
echo ""
TEST_OPENAI_API_URL="$UPSTREAM_URL" cargo test --test integration_test -- --test-threads=1 --nocapture
