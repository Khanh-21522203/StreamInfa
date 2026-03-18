# Feature: Live Ingest over RTMP

## 1. Purpose

Accept live publishers over RTMP, authenticate by stream key, demux FLV payloads, and forward normalized media frames to the transcode pipeline with bounded backpressure.

## 2. Responsibilities

- Listen on RTMP port (`:1935` by default)
- Complete RTMP handshake and command flow (`connect`, `createStream`, `publish`)
- Validate stream key before accepting publish
- Parse FLV tags and emit normalized audio/video frames
- Handle disconnects and map ingest errors to lifecycle events

## 3. Non-Responsibilities

- No HLS packaging
- No long-term archive storage
- No ingest protocol support beyond RTMP in MVP

## 4. Architecture Design

```
RTMP TCP listener
  -> session handshake
  -> command state machine
  -> FLV demux
  -> frame normalization
  -> bounded channel to transcode stage
```

## 5. Core Data Structures (Rust)

- `RtmpSession` (connection state + stream context)
- `DemuxedFrame` (`pts`, `dts`, `track`, payload bytes)
- `MediaInfo` extracted from early packets
- `IngestError` variants (auth, protocol, codec, io)

## 6. Public Interfaces

- Publish URL form:
  - `rtmp://{host}:1935/live/{stream_key}`
- Internal:
  - `start_rtmp_listener(config, event_tx, frame_tx)`
  - `handle_publish(session, stream_key)`

## 7. Internal Algorithms

1. Accept TCP connection and read C0/C1, return S0/S1/S2.
2. Process command messages and require valid `publish`.
3. Resolve stream key to stream id via control-plane registry.
4. Demux FLV tags; reject unsupported codecs early.
5. Normalize timestamps (stable monotonic base).
6. Push frames into bounded queue; if full, apply backpressure chain.
7. On disconnect or fatal parse error, emit stream-ended/error event.

## 8. Persistence Model

- No ingest payload persistence in this module.
- Frame data is transient and forwarded downstream.

## 9. Concurrency Model

- One async task per RTMP session
- Shared lookup for stream-key auth
- Bounded channels to avoid unbounded memory growth

## 10. Configuration

- RTMP bind address/port
- Max concurrent RTMP sessions
- Ingest queue capacity
- Read timeout / idle timeout

## 11. Observability

- Connection counters (`accepted`, `rejected`, `active`)
- Auth failures by reason
- Frame ingest rate and queue utilization
- Session duration and disconnect reason logs

## 12. Testing Strategy

- Unit: handshake parser, FLV demux parser, key validation
- Integration: FFmpeg push to local listener with expected transitions
- Failure injection: malformed RTMP packets, disconnect mid-stream
- Load: multiple concurrent live streams under bounded queues

## 13. Implementation Plan

1. Implement RTMP listener + session state machine.
2. Implement stream-key auth integration with control plane.
3. Implement FLV demux and codec gate checks (H.264/AAC/MP3 input rules).
4. Implement timestamp normalization and frame emission.
5. Add backpressure-aware channel writes and timeout handling.
6. Emit lifecycle events and map errors to typed failure causes.
7. Add ingest metrics and integration tests with FFmpeg fixture stream.

## 14. Open Questions

- Whether to include optional IP allow-listing on RTMP endpoint in MVP or Phase 2.
