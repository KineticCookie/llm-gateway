.PHONY: help build release test docker-build docker-run docker-clean clean fmt lint

help: ## Show this help message
	@echo 'Usage: make [target]'
	@echo ''
	@echo 'Available targets:'
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z_-]+:.*?## / {printf "  %-20s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

build: ## Build debug binary
	cargo build

release: ## Build optimized release binary
	cargo build --release

test: ## Run tests
	cargo test

fmt: ## Format code
	cargo fmt

lint: ## Run clippy linter
	cargo clippy -- -D warnings

clean: ## Clean build artifacts
	cargo clean

docker-image: ## Build Docker image
	docker build -t llm-gateway:latest .

docker-image-amd64: ## Build Docker image
	docker build --platform linux/amd64 -t llm-gateway:latest .

run: ## Run locally in development mode
	cargo run

run-release: release ## Run release binary locally
	./target/release/llm-gateway
