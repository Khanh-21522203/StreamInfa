# ============================================================
# Stage 1: Build FFmpeg with minimal codec support
# (from docker.md §1.2)
# ============================================================
FROM ubuntu:22.04 AS ffmpeg-builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    pkg-config \
    yasm \
    nasm \
    git \
    wget \
    libx264-dev \
    libfdk-aac-dev \
    && rm -rf /var/lib/apt/lists/*

ARG FFMPEG_VERSION=6.1.1

RUN wget -q https://ffmpeg.org/releases/ffmpeg-${FFMPEG_VERSION}.tar.xz \
    && tar xf ffmpeg-${FFMPEG_VERSION}.tar.xz \
    && cd ffmpeg-${FFMPEG_VERSION} \
    && ./configure \
        --prefix=/usr/local \
        --enable-gpl \
        --enable-libx264 \
        --enable-libfdk-aac \
        --enable-nonfree \
        --enable-shared \
        --disable-static \
        --disable-programs \
        --disable-doc \
        --disable-network \
        --disable-autodetect \
        --enable-small \
        --disable-avdevice \
        --disable-postproc \
        --disable-swresample \
        --enable-swscale \
    && make -j$(nproc) \
    && make install \
    && ldconfig

# ============================================================
# Stage 2: Build StreamInfa Rust binary
# (from docker.md §1.2)
# ============================================================
FROM rust:1.77-bookworm AS rust-builder

# Copy FFmpeg libraries and headers from stage 1
COPY --from=ffmpeg-builder /usr/local/lib/libav* /usr/local/lib/
COPY --from=ffmpeg-builder /usr/local/lib/libsw* /usr/local/lib/
COPY --from=ffmpeg-builder /usr/local/lib/pkgconfig/ /usr/local/lib/pkgconfig/
COPY --from=ffmpeg-builder /usr/local/include/libav* /usr/local/include/
COPY --from=ffmpeg-builder /usr/local/include/libsw* /usr/local/include/

# Install system dependencies for linking
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libx264-dev \
    libfdk-aac-dev \
    libclang-dev \
    && rm -rf /var/lib/apt/lists/*

RUN ldconfig

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

# Copy FFmpeg shared libraries
COPY --from=ffmpeg-builder /usr/local/lib/libav*.so* /usr/local/lib/
COPY --from=ffmpeg-builder /usr/local/lib/libsw*.so* /usr/local/lib/
RUN ldconfig

# Copy StreamInfa binary
COPY --from=rust-builder /app/target/release/streaminfa /usr/local/bin/streaminfa

# Copy default configuration
COPY config/default.toml /etc/streaminfa/default.toml

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
CMD ["--config", "/etc/streaminfa/default.toml"]
