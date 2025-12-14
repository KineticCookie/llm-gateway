# Multi-stage build for minimal image size
FROM rust:1.92-alpine AS builder

# Install build dependencies including gcc for OpenSSL
RUN apk add --no-cache \
    musl-dev \
    gcc \
    openssl-dev \
    openssl-libs-static \
    pkgconfig \
    perl \
    make

WORKDIR /app

# Copy manifests
COPY Cargo.toml Cargo.lock ./

# Copy source code
COPY src ./src

# Build release binary (standard, not musl)
ENV OPENSSL_STATIC=yes
ENV OPENSSL_DIR=/usr
RUN cargo build --release

# Runtime stage - minimal Alpine
FROM alpine:latest

# Install only runtime essentials
RUN apk add --no-cache \
    ca-certificates \
    curl

WORKDIR /app

# Copy binary from builder
COPY --from=builder /app/target/release/llm-gateway /app/llm-gateway

# Copy config example (users can mount their own)
COPY config.example.yaml /app/config.example.yaml

# Expose default port
EXPOSE 8080

# Health check
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:8080/health || exit 1

# Run as non-root user
RUN adduser -D -u 1000 proxy && chown -R proxy:proxy /app
USER proxy

# Set environment variables
ENV RUST_LOG=info

# Run the binary
CMD ["/app/llm-gateway"]
