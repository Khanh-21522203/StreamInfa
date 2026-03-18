# Feature: Security and Input Hardening

## 1. Purpose

Protect ingest and control surfaces against unauthorized access and common abuse paths while preserving MVP simplicity.

## 2. Responsibilities

- Enforce bearer auth for `/api/v1/*`
- Enforce stream-key authentication for RTMP publish
- Validate upload/input constraints to reduce crash/abuse risk
- Redact secrets/sensitive fields in logs
- Define baseline network exposure and TLS posture

## 3. Non-Responsibilities

- No playback authorization in MVP
- No full RBAC/tenant isolation in MVP
- No DRM key management

## 4. Architecture Design

```
Request
 -> auth middleware (API token / stream key)
 -> validation layer (size, codec, format, IDs)
 -> feature handler
 -> sanitized logging
```

## 5. Core Data Structures (Rust)

- `AuthConfig` (admin tokens, token sources)
- `StreamKeyRegistry` (stream_id -> active key)
- `ValidationPolicy` (size limits, codec allowlist, CORS policy)
- `SecurityError` types mapped to HTTP/ingest error responses

## 6. Public Interfaces

- API bearer auth on all `/api/v1/*` endpoints
- RTMP publish path requiring valid stream key
- Key-rotation endpoint:
  - `POST /api/v1/streams/{id}/rotate-key`

## 7. Internal Algorithms

1. Verify bearer token using constant-time comparison.
2. Verify RTMP stream key lookup and active-key match.
3. Reject malformed payloads and unsupported codecs early.
4. Enforce request size and timeout guards.
5. Redact secret fields before structured log emission.
6. Rotate stream key atomically while preserving existing active sessions.

## 8. Persistence Model

- MVP secrets sourced from config/environment only.
- Stream keys stored in memory with stream records.

## 9. Concurrency Model

- Read-heavy token/key checks with lock-free or RW-safe access
- Atomic swap for rotated keys
- Middleware-safe stateless verification paths

## 10. Configuration

- Admin token values and rotation policy
- CORS allowed origins/methods/headers
- Max request/upload sizes and validation limits
- TLS termination strategy (edge/proxy in MVP)

## 11. Observability

- Auth failure counters by endpoint/protocol/reason
- Security validation rejection counters
- Warning logs for repeated invalid auth attempts
- Audit logs for key rotation and admin actions

## 12. Testing Strategy

- Unit: token verify, key lookup, redaction filters
- Integration: API auth required paths and RTMP auth gating
- Security regression: malformed payload fuzzing at parser boundaries
- Ops test: key-rotation behavior with active publisher session

## 13. Implementation Plan

1. Implement auth middleware for control API and shared error model.
2. Implement stream-key auth in RTMP publish handshake path.
3. Implement validation policy layer for upload/RTMP/API payloads.
4. Implement redaction utility and ensure structured logging uses it.
5. Implement key-rotation endpoint semantics and cache updates.
6. Add security metrics and alert hooks for auth failure spikes.
7. Validate with integration/security regression test suite.

## 14. Open Questions

- Whether to add simple IP-based allow/deny controls in MVP runtime config.
