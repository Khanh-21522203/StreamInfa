# StreamInfa - Docker and Local Development

> **Purpose:** Define the local containerized workflow for running and validating StreamInfa.
> **Audience:** Engineers implementing or testing StreamInfa locally.

---

## 1. Prerequisites

1. Docker Engine (24+ recommended).
2. Docker Compose V2 (`docker compose` command).
3. FFmpeg installed locally if you want to push RTMP from host tools.
4. At least 8 GB RAM and 20 GB free disk for local media/test artifacts.

---

## 2. Local Stack Topology

Expected local stack:

1. `streaminfa` service (app container).
2. `minio` service (S3-compatible object store).
3. Optional observability stack (Prometheus/Grafana/Loki) if enabled by your compose profile.

Default ports:

1. `1935` - RTMP ingest.
2. `8080` - control API + delivery origin.
3. `9000` - MinIO API.
4. `9001` - MinIO console (if enabled).

---

## 3. Bootstrapping Commands

From repository root:

```bash
# Build and start the local stack
docker compose -f docker/docker-compose.yml up -d --build

# Check container status
docker compose -f docker/docker-compose.yml ps

# Tail application logs
docker compose -f docker/docker-compose.yml logs -f streaminfa
```

Stop and remove containers:

```bash
docker compose -f docker/docker-compose.yml down
```

---

## 4. Minimal Runtime Configuration

Set these environment variables (directly or via `.env` consumed by compose):

```bash
STREAMINFA_ENV=development
STREAMINFA_STORAGE_ENDPOINT=http://minio:9000
STREAMINFA_STORAGE_BUCKET=streaminfa-media
STREAMINFA_STORAGE_ACCESS_KEY_ID=minioadmin
STREAMINFA_STORAGE_SECRET_ACCESS_KEY=minioadmin
STREAMINFA_AUTH_ADMIN_TOKENS=at_local_dev_token_1
```

If your compose file maps different names or values, use those values consistently in API calls and smoke tests.

---

## 5. Smoke Test Checklist

### 5.1 Service Health

```bash
curl -s http://localhost:8080/healthz
curl -s http://localhost:8080/readyz
curl -s http://localhost:8080/metrics | head
```

### 5.2 Live Ingest and Playback

```bash
# 1) Create a stream
curl -s -X POST http://localhost:8080/api/v1/streams \
  -H "Authorization: Bearer at_local_dev_token_1" \
  -H "Content-Type: application/json" \
  -d '{"ingest_type":"rtmp","metadata":{"title":"dev-live"}}'

# 2) Push media from ffmpeg (replace STREAM_KEY)
ffmpeg -re -stream_loop -1 -i fixtures/test_h264_aac.mp4 \
  -c copy -f flv rtmp://localhost:1935/live/STREAM_KEY

# 3) Fetch playback manifest (replace STREAM_ID)
curl -i http://localhost:8080/streams/STREAM_ID/master.m3u8
```

### 5.3 VOD Upload

```bash
curl -s -X POST http://localhost:8080/api/v1/streams/upload \
  -H "Authorization: Bearer at_local_dev_token_1" \
  -F "file=@fixtures/test_h264_aac.mp4"
```

---

## 6. Integration and E2E Test Execution

```bash
# Unit tests
cargo test --lib --bins

# Integration tests (in-process control-plane contracts)
cargo test --test integration_control_plane -- --nocapture

# E2E smoke (in-process playback flow)
cargo test --test e2e_playback_flow -- --nocapture

# Full suite
cargo test --all -- --nocapture
```

---

## 7. Troubleshooting

1. `readyz` fails storage check:
   - Verify MinIO container is healthy.
   - Verify bucket exists and credentials match env config.
2. RTMP publish fails auth:
   - Confirm stream key from `POST /api/v1/streams` response.
   - Check logs for auth rejection reasons.
3. Playback 404 on segments:
   - Confirm stream reached `live` or `ready`.
   - Check storage write failures in logs/metrics.
4. High CPU / transcode stalls:
   - Reduce concurrent streams in local tests.
   - Use lower-resolution input fixtures.
