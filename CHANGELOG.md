# Changelog

All notable changes to this project are documented here. The format is based
on [Keep a Changelog](https://keepachangelog.com/), and the project follows
[Semantic Versioning](https://semver.org/).

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
