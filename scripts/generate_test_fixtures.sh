#!/bin/bash
# StreamInfa — Test Fixture Generation Script
# (from testing-strategy.md §9.1)
#
# Generates all test media fixtures using FFmpeg.
# This script is idempotent — safe to run multiple times.
#
# Usage:
#   ./scripts/generate_test_fixtures.sh
#
# Requirements:
#   - FFmpeg 6.x installed
#   - ~50 MB disk space for fixtures

set -euo pipefail

FIXTURES_DIR="fixtures"
mkdir -p "$FIXTURES_DIR"

echo "=== StreamInfa Test Fixture Generator ==="
echo "Output directory: $FIXTURES_DIR"
echo ""

# Check FFmpeg is available
if ! command -v ffmpeg &> /dev/null; then
    echo "ERROR: ffmpeg not found. Install FFmpeg 6.x first."
    exit 1
fi

echo "FFmpeg version: $(ffmpeg -version | head -1)"
echo ""

# --------------------------------------------------------
# 1. test_h264_aac.mp4 — 10s 1080p H.264 + AAC (~5 MB)
#    Standard happy-path test
# --------------------------------------------------------
echo "[1/8] Generating test_h264_aac.mp4 (10s, 1080p, H.264+AAC)..."
ffmpeg -y -f lavfi -i "color=c=blue:s=1920x1080:d=10:r=30" \
       -f lavfi -i "sine=frequency=440:duration=10:sample_rate=48000" \
       -c:v libx264 -profile:v high -level 4.1 -b:v 4000k -g 60 \
       -c:a aac -b:a 128k \
       "$FIXTURES_DIR/test_h264_aac.mp4" 2>/dev/null

# --------------------------------------------------------
# 2. test_h264_aac_720p.mp4 — 10s 720p H.264 + AAC (~3 MB)
#    Test rendition selection (skip 1080p)
# --------------------------------------------------------
echo "[2/8] Generating test_h264_aac_720p.mp4 (10s, 720p, H.264+AAC)..."
ffmpeg -y -f lavfi -i "color=c=green:s=1280x720:d=10:r=30" \
       -f lavfi -i "sine=frequency=880:duration=10:sample_rate=48000" \
       -c:v libx264 -profile:v main -level 3.1 -b:v 2000k -g 60 \
       -c:a aac -b:a 128k \
       "$FIXTURES_DIR/test_h264_aac_720p.mp4" 2>/dev/null

# --------------------------------------------------------
# 3. test_h264_aac.flv — 10s 1080p FLV H.264 + AAC (~5 MB)
#    RTMP ingest simulation
# --------------------------------------------------------
echo "[3/8] Generating test_h264_aac.flv (10s, 1080p, FLV)..."
ffmpeg -y -f lavfi -i "color=c=red:s=1920x1080:d=10:r=30" \
       -f lavfi -i "sine=frequency=440:duration=10:sample_rate=48000" \
       -c:v libx264 -profile:v high -level 4.1 -b:v 4000k -g 60 \
       -c:a aac -b:a 128k \
       -f flv "$FIXTURES_DIR/test_h264_aac.flv" 2>/dev/null

# --------------------------------------------------------
# 4. test_h264_mp3.mp4 — 5s 720p H.264 + MP3 (~2 MB)
#    Test MP3→AAC re-encode
# --------------------------------------------------------
echo "[4/8] Generating test_h264_mp3.mp4 (5s, 720p, H.264+MP3)..."
ffmpeg -y -f lavfi -i "color=c=yellow:s=1280x720:d=5:r=30" \
       -f lavfi -i "sine=frequency=660:duration=5:sample_rate=48000" \
       -c:v libx264 -profile:v main -level 3.1 -b:v 2000k -g 60 \
       -c:a libmp3lame -b:a 128k \
       "$FIXTURES_DIR/test_h264_mp3.mp4" 2>/dev/null

# --------------------------------------------------------
# 5. test_corrupt.mp4 — Corrupted file (~1 MB)
#    Test corrupt file handling
# --------------------------------------------------------
echo "[5/8] Generating test_corrupt.mp4 (corrupted)..."
# Create a valid file first, then corrupt it
ffmpeg -y -f lavfi -i "color=c=black:s=640x480:d=2:r=30" \
       -c:v libx264 -b:v 500k -g 30 \
       "$FIXTURES_DIR/test_corrupt_base.mp4" 2>/dev/null
# Corrupt by overwriting middle bytes with random data
cp "$FIXTURES_DIR/test_corrupt_base.mp4" "$FIXTURES_DIR/test_corrupt.mp4"
dd if=/dev/urandom of="$FIXTURES_DIR/test_corrupt.mp4" bs=1 count=1024 seek=4096 conv=notrunc 2>/dev/null
rm -f "$FIXTURES_DIR/test_corrupt_base.mp4"

# --------------------------------------------------------
# 6. test_no_audio.mp4 — 5s 720p video-only (~2 MB)
#    Test video-only stream
# --------------------------------------------------------
echo "[6/8] Generating test_no_audio.mp4 (5s, 720p, video-only)..."
ffmpeg -y -f lavfi -i "color=c=cyan:s=1280x720:d=5:r=30" \
       -c:v libx264 -profile:v main -level 3.1 -b:v 2000k -g 60 \
       -an \
       "$FIXTURES_DIR/test_no_audio.mp4" 2>/dev/null

# --------------------------------------------------------
# 7. test_long.mp4 — 60s 480p H.264 + AAC (~10 MB)
#    Test multi-segment output
# --------------------------------------------------------
echo "[7/8] Generating test_long.mp4 (60s, 480p, H.264+AAC)..."
ffmpeg -y -f lavfi -i "color=c=magenta:s=854x480:d=60:r=30" \
       -f lavfi -i "sine=frequency=440:duration=60:sample_rate=48000" \
       -c:v libx264 -profile:v main -level 3.0 -b:v 1000k -g 60 \
       -c:a aac -b:a 96k \
       "$FIXTURES_DIR/test_long.mp4" 2>/dev/null

# --------------------------------------------------------
# 8. test_hevc_aac.mp4 — 5s 1080p HEVC + AAC (~3 MB)
#    Test codec rejection (HEVC not supported)
# --------------------------------------------------------
echo "[8/8] Generating test_hevc_aac.mp4 (5s, 1080p, HEVC+AAC)..."
if ffmpeg -encoders 2>/dev/null | grep -q libx265; then
    ffmpeg -y -f lavfi -i "color=c=white:s=1920x1080:d=5:r=30" \
           -f lavfi -i "sine=frequency=440:duration=5:sample_rate=48000" \
           -c:v libx265 -b:v 3000k -g 60 \
           -c:a aac -b:a 128k \
           "$FIXTURES_DIR/test_hevc_aac.mp4" 2>/dev/null
else
    echo "  SKIPPED: libx265 not available. Install libx265-dev to generate this fixture."
fi

echo ""
echo "=== Fixture generation complete ==="
echo ""
echo "Files generated:"
ls -lh "$FIXTURES_DIR"/*.mp4 "$FIXTURES_DIR"/*.flv 2>/dev/null || true
echo ""
echo "Total size:"
du -sh "$FIXTURES_DIR"
