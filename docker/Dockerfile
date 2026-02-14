# ============================================================
# Stage 1: Install pre-built FFmpeg packages
# ============================================================
FROM ubuntu:22.04 AS ffmpeg-builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    ffmpeg \
    libavcodec-dev \
    libavformat-dev \
    libavutil-dev \
    libswscale-dev \
    libx264-dev \
    libfdk-aac-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

# ============================================================
# Stage 2: Build StreamInfa Rust binary
# ============================================================
FROM rust:1.82-bookworm AS rust-builder

WORKDIR /app

# Cache dependencies: copy only Cargo files first
COPY Cargo.toml Cargo.lock ./

# Create dummy src to cache dependency compilation
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs

RUN cargo build --release 2>/dev/null || true

# Now copy actual source and rebuild
COPY src/ src/
COPY config/ config/

# Touch source files to invalidate the dummy build
RUN find src/ -name "*.rs" -exec touch {} +

RUN cargo build --release --bin streaminfa

# Strip debug symbols
RUN strip /app/target/release/streaminfa

# ============================================================
# Stage 3: Minimal runtime image
# (from docker.md §1.2)
# Target size: < 200 MB
# ============================================================
FROM ubuntu:22.04 AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    libx264-163 \
    libfdk-aac2 \
    && rm -rf /var/lib/apt/lists/*

# Copy StreamInfa binary
COPY --from=rust-builder /app/target/release/streaminfa /usr/local/bin/streaminfa

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
CMD ["--config", "/app/config/default.toml"]
