#!/bin/bash

# Cleanup function to stop docker compose on exit
cleanup() {
    echo ""
    echo "Shutting down observability stack..."
    docker compose down
    exit 0
}

# Trap SIGINT (Ctrl-C) and SIGTERM
trap cleanup SIGINT SIGTERM

# Create logs directory if it doesn't exist
mkdir -p logs

# Start the observability stack
echo "Starting observability stack..."
docker compose up --build -d

# if docker compose fails, exit the script
if [ $? -ne 0 ]; then
    echo "Failed to start observability stack. Exiting."
    exit 1
fi

# Wait for services to be ready
echo "Waiting for services to start..."
sleep 5

# Check if services are running
docker compose ps

echo ""
echo "Observability stack started!"
echo "- Grafana:    http://localhost:3000 (admin/admin)"
echo "- Prometheus: http://localhost:9090"
echo "- Loki:       http://localhost:3100"
echo ""
echo "Starting proxy with logging..."
echo "Logs will be written to: logs/proxy.log"
echo ""

# Run the proxy with logging
# RUST_LOG=info cargo run --release 2>&1 | tee -a logs/proxy.log

docker compose logs -f