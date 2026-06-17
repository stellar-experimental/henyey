# Henyey Docker image for Stellar Supercluster (SSC) integration.
#
# Produces a minimal container with the henyey binary symlinked as
# `stellar-core` so SSC can use it as a drop-in replacement.
#
# Build:
#   docker build -t henyey:latest .
#
# Build with jemalloc (recommended for production):
#   docker build --build-arg FEATURES=jemalloc -t henyey:latest .
#
# Run (SSC-style, config injected via volume mount):
#   docker run -v /path/to/stellar-core.cfg:/etc/stellar/stellar-core.cfg \
#     -p 11625:11625 -p 11626:11626 \
#     henyey:latest run --conf /etc/stellar/stellar-core.cfg
#
# SSC passes subcommands as container arguments (new-db, run, catchup, etc.)
# since the ENTRYPOINT is the binary itself.

# ── Build stage ──────────────────────────────────────────────────────────
# Track the latest stable Rust on bookworm. CI builds with the stable toolchain
# (`dtolnay/rust-toolchain@stable`), so the image must use a Rust new enough for
# whatever the checked-in Cargo.lock resolves to (some deps require >= 1.88).
# Pinning a specific minor (e.g. 1.86) goes stale as the lockfile advances.
FROM rust:1-bookworm AS builder

ARG FEATURES=""

WORKDIR /build

# Install build dependencies (sqlite3 for rusqlite, pkg-config for linking)
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libsqlite3-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy workspace manifests first for dependency caching
COPY Cargo.toml Cargo.lock* ./
COPY crates/ crates/


# Build the release binary
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    if [ -n "$FEATURES" ]; then \
        cargo build --release --bin henyey --features "$FEATURES"; \
    else \
        cargo build --release --bin henyey; \
    fi && \
    cp /build/target/release/henyey /usr/local/bin/henyey

# ── Runtime stage ────────────────────────────────────────────────────────
FROM debian:bookworm-slim

# Install minimal runtime dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    libsqlite3-0 \
    curl \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy binary and create stellar-core symlink for SSC compatibility
COPY --from=builder /usr/local/bin/henyey /usr/bin/henyey
RUN ln -s /usr/bin/henyey /usr/bin/stellar-core

# Create data directories matching stellar-core conventions
RUN mkdir -p /data /opt/stellar /etc/stellar

# Stellar peer-to-peer port
EXPOSE 11625
# stellar-core admin HTTP port (compat server)
EXPOSE 11626

VOLUME ["/data"]
WORKDIR /etc/stellar

# SSC passes subcommands (run, new-db, catchup, etc.) as container args.
# Use the stellar-core symlink as entrypoint for SSC compatibility.
ENTRYPOINT ["/usr/bin/stellar-core"]
