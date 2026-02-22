# ── builder ───────────────────────────────────────────────────────────────────
FROM rust:1.88-slim AS builder

# Install build dependencies for native TLS and linking
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Cache dependency compilation separately from application code.
# Copy only manifests first for better layer caching.
COPY Cargo.toml Cargo.lock ./

# Build a dummy main so Cargo fetches and compiles all deps.
# This layer is cached unless Cargo.toml/Cargo.lock change.
RUN mkdir src && \
    echo 'fn main(){}' > src/main.rs && \
    cargo build --release && \
    rm -rf src

# Now build the real source.
COPY src ./src

# Touch main.rs to invalidate the dummy build, then build for real.
# Use release profile with optimizations.
RUN touch src/main.rs && \
    cargo build --release && \
    strip target/release/harbor-router

# ── runtime ───────────────────────────────────────────────────────────────────
# Use distroless for minimal attack surface (~2MB base)
FROM gcr.io/distroless/cc-debian12:nonroot

# Copy only the binary
COPY --from=builder /build/target/release/harbor-router /harbor-router

# Metadata
LABEL org.opencontainers.image.source="https://github.com/p2p-org/harbor-router"
LABEL org.opencontainers.image.description="Harbor proxy-cache router"

EXPOSE 8080 9090

# Run as non-root (distroless:nonroot runs as uid 65532)
USER nonroot:nonroot

ENTRYPOINT ["/harbor-router"]
