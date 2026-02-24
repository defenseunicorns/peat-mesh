# Multi-stage build for eche-mesh-node
# Builder uses mold linker per .cargo/config.toml

FROM rust:1.93-bookworm AS builder

# Install build tools required by .cargo/config.toml (clang + mold linker)
RUN apt-get update && apt-get install -y --no-install-recommends \
    clang mold \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy manifests first for dependency caching
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo

# Create dummy sources to cache dependencies
RUN mkdir -p src/bin examples && \
    echo "fn main() {}" > src/bin/eche-mesh-node.rs && \
    echo "// dummy" > src/lib.rs && \
    echo "fn main() {}" > examples/basic_mesh.rs && \
    echo "fn main() {}" > examples/document_sync.rs && \
    echo "fn main() {}" > examples/broker_service.rs && \
    cargo build --release --bin eche-mesh-node --features node 2>/dev/null || true

# Copy real source and build
COPY src src
COPY examples examples
RUN touch src/lib.rs src/bin/eche-mesh-node.rs && \
    cargo build --release --bin eche-mesh-node --features node

# ── Runtime ──────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates tini curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/eche-mesh-node /usr/local/bin/eche-mesh-node

# Broker HTTP/WS
EXPOSE 8081/tcp
# Iroh QUIC
EXPOSE 11204/udp

ENTRYPOINT ["tini", "--"]
CMD ["eche-mesh-node"]
