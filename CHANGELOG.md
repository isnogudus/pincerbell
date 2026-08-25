# Changelog

All notable changes to this project are documented here. The format is based
on [Keep a Changelog](https://keepachangelog.com/), and the project follows
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- Queue/poll relay for split deployments where the homeserver-facing system
  cannot reach the push services: a queue-side instance (`[queue]`) buffers
  notifications in a bounded in-memory ring buffer and serves them over
  HTTPS long-poll (`/_pincerbell/v1/poll` + `/_pincerbell/v1/ack`, shared
  bearer token); a poll-side instance (`[[poll]]`, several upstreams
  supported) fetches outbound and delivers via its configured apps.
  - The app list lives on the poll side only: with `[queue]` configured,
    every app_id without an explicit `[apps]` entry is queued (explicit
    entries still deliver directly) and `reject_unknown_apps` has no
    effect on the queue side.
  - At-least-once via lease/ack — un-acknowledged entries redeliver after
    the lease expires, the existing duplicate suppression absorbs the
    repeats; transient poll-side failures simply leave the entry unacked.
  - Count-only notifications coalesce per device to the newest badge
    state; event content never crosses the relay.
  - Invalid pushkeys reported by the poll side answer `rejected` on the
    homeserver's *next* notify — one delivery attempt later, the earliest
    the push-gateway API allows.
  - The buffer is deliberately in-memory and non-persistent: pushes are
    contentless wake-up signals, clients resync on next open, and a
    restart merely costs a wake-up.
- Per-mode example configurations (`pincerbell.toml.example` for direct
  operation, `pincerbell-queue.toml.example` / `pincerbell-poll.toml.example`
  for the relay roles, all parse-checked by a test) and a worked relay
  deployment walkthrough in `docs/queue-poll-relay.md`.

## [0.1.0] - 2026-07-24

First release. An independent Matrix Push Gateway, implemented from scratch
against the public [Push Gateway API
specification](https://spec.matrix.org/latest/push-gateway-api/) — not a
port of any other gateway.

### Added
- `POST /_matrix/push/v1/notify` with spec-faithful wire types, plus a
  `/health` endpoint. Devices of one notification are delivered
  concurrently.
- Duplicate suppression for homeserver retries: a TTL-bounded, size-capped
  in-memory record of delivered (event_id, app_id, pushkey) triples. Only
  successful deliveries are recorded, so a failed attempt never shadows its
  own retry; count-only notifications are idempotent and always pass.
- Delivery backends, configured per app_id in `pincerbell.toml`:
  - **FCM** (Firebase Cloud Messaging, HTTP v1): OAuth2 via the
    service-account JWT flow, data-only messages.
  - **APNs** (HTTP/2 provider API): token-based auth from a .p8 signing
    key; `alert` pushes with a rewritable fallback alert
    (`mutable-content`) and PushKit `voip` pushes at full priority.
  - **Web Push** (RFC 8030/8291/8292): VAPID authorization and aes128gcm
    payload encryption, verified against RFC 8291's own test vector; a
    mandatory `allowed_endpoints` allowlist guards against SSRF.
  - **Log sink** for development and testing.
- Push payloads carry notification metadata only — event content is never
  forwarded to any push service, and message content is never logged.
- Conservative rejection policy throughout: only a definitive
  "token/subscription gone" answer (FCM `UNREGISTERED`, APNs
  `Unregistered`, Web Push 404/410) rejects a pushkey; transient failures
  answer 502 so the homeserver retries.
- Docker packaging: static musl binary on bare Alpine, compose example,
  `keys/` service directory for credential files (gitignored and excluded
  from the image).

[0.1.0]: https://github.com/isnogudus/pincerbell/releases/tag/v0.1.0
