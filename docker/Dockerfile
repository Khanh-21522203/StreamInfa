# ============================================================
# Stage 1: Build StreamInfa with FFmpeg + S3 support
# ============================================================
FROM rust:1.91-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    clang \
    libavcodec-dev \
    libavformat-dev \
    libavutil-dev \
    libavfilter-dev \
    libavdevice-dev \
    libswscale-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Fetch dependencies first (cache layer)
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src benches \
    && echo 'fn main() {}' > src/main.rs \
    && echo 'fn main() {}' > benches/pipeline_micro.rs \
    && cargo fetch

# Build with real source
COPY src/ src/
COPY benches/ benches/
COPY config/ config/
RUN find src/ benches/ -name "*.rs" -exec touch {} +
RUN cargo build --release --bin streaminfa --features s3,ffmpeg

# Strip debug symbols
RUN strip /app/target/release/streaminfa

# ============================================================
# Stage 2: Minimal runtime image
# (from docker.md §1.2)
# Target size: < 200 MB
# ============================================================
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    ffmpeg \
    && rm -rf /var/lib/apt/lists/*

# Copy StreamInfa binary
COPY --from=builder /app/target/release/streaminfa /usr/local/bin/streaminfa

WORKDIR /app

# Copy default configuration
COPY config/default.toml /app/config/default.toml

# Create non-root user (from security best practices)
RUN groupadd -r streaminfa && useradd -r -g streaminfa -m streaminfa

# Create temp directory for uploads
RUN mkdir -p /tmp/streaminfa/uploads && chown streaminfa:streaminfa /tmp/streaminfa/uploads

USER streaminfa

# Expose ports (from docker.md §1.2)
# 1935 = RTMP ingest
# 8080 = HTTP (control + delivery + metrics)
EXPOSE 1935 8080

# Health check (from docker.md §1.2)
HEALTHCHECK --interval=10s --timeout=3s --start-period=15s --retries=3 \
    CMD curl -f http://localhost:8080/healthz || exit 1

ENTRYPOINT ["streaminfa"]
