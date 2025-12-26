# Stage 1: Builder
FROM rust:1.92-slim AS builder

# Install dependencies needed for building reqwest (native-tls)
RUN apt-get update && \
    apt-get install -y pkg-config libssl-dev && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy source code
COPY Cargo.toml Cargo.lock ./

# Build dependencies to cache them
RUN mkdir src && \
    echo 'fn main() {}' > src/main.rs && \
    cargo build --release && \
    rm -rf src

# Copy the actual source code
COPY src ./src

# Build the application, this will use the cached dependencies
RUN cargo build --release

# Stage 2: Runtime
FROM debian:bookworm-slim

# Install ca-certificates to allow HTTPS calls
RUN apt-get update && \
    apt-get install -y ca-certificates && \
    rm -rf /var/lib/apt/lists/*

# Create a non-root user
RUN useradd -m appuser

WORKDIR /app

# Copy the binary from the builder stage
COPY --from=builder /app/target/release/llm-reasoning-proxy /app/llm-reasoning-proxy

# Switch to the non-root user
USER appuser

# Expose the default port
EXPOSE 5001

# Set the entry point
# Users can override arguments (e.g., --port) using Docker CMD
ENTRYPOINT ["/app/llm-reasoning-proxy"]
