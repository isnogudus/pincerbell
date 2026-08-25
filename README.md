# pincerbell

[![CI](https://github.com/isnogudus/pincerbell/actions/workflows/ci.yml/badge.svg)](https://github.com/isnogudus/pincerbell/actions/workflows/ci.yml)
[![GitHub release](https://img.shields.io/github/v/release/isnogudus/pincerbell)](https://github.com/isnogudus/pincerbell/releases)
[![crates.io](https://img.shields.io/crates/v/pincerbell.svg)](https://crates.io/crates/pincerbell)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

An independent Matrix Push Gateway implementation in Rust.

`pincerbell` implements the [Matrix Push Gateway API](https://spec.matrix.org/latest/push-gateway-api/)
so that Matrix homeservers can deliver push notifications to client apps.
It is written from scratch against the public specification and is not a
port of, based on, or affiliated with any other Push Gateway implementation
(such as Element's [Sygnal](https://github.com/element-hq/sygnal)) or with
the Matrix.org Foundation.

**Status:** early stage. The `POST /_matrix/push/v1/notify` endpoint is
implemented with duplicate suppression for homeserver retries and four
delivery backends: **FCM** (Firebase Cloud Messaging, HTTP v1 API, via a
Google service account), **APNs** (Apple Push Notification service, HTTP/2
provider API, token-based auth via a .p8 key, alert and PushKit VoIP push
types), **Web Push** (RFC 8030 with VAPID authorization per RFC 8292 and
RFC 8291 payload encryption — verified against the RFC's own test vector),
and a **log sink** for development and testing. For deployments where the
homeserver-facing system cannot reach the push services, a [queue/poll
relay](#queuepoll-relay) splits the gateway into a buffering edge instance
and a delivering instance that fetches over HTTPS long-poll.

Pushes never carry the event content — only notification metadata
(event_id, room_id, type, sender, ...); the client app fetches the event
itself. On FCM that means data-only messages; on APNs an alert with
`mutable-content: 1` whose fallback text the app's notification service
extension replaces after fetching the event (count-only notifications
become badge-only updates); on Web Push the metadata is the encrypted
JSON payload. A pushkey is only reported as rejected on FCM
`UNREGISTERED` / APNs `Unregistered` / Web Push 404/410; transient errors
fail the request with 502 so the homeserver retries (already-delivered
devices are shielded by the duplicate suppression).

Web Push subscription endpoints are client-controlled, so the `webpush`
app type requires an explicit `allowed_endpoints` allowlist of push-service
hosts — without it the gateway would be an SSRF proxy.

## Queue/poll relay

When the system the homeserver can reach and the system that can reach the
push services are not the same (restricted hosting, egress-only networks),
pincerbell can split into two instances of the same binary:

- **Queue side**, next to the homeserver: accepts `/notify` as usual but
  holds notifications in a bounded in-memory ring buffer (`[queue]` in the
  config). It needs no route to any push service. Every app_id without an
  explicit `[apps]` entry is queued — the app list lives on the poll side
  only.
- **Poll side**, on the delivering network: fetches entries outbound via
  HTTPS long-poll (`POST /_pincerbell/v1/poll`, one `[[poll]]` table per
  upstream — one poll side can serve several queue sides), delivers them
  through its configured apps, and acknowledges (`/_pincerbell/v1/ack`).
  It needs no inbound port.

Both sides authenticate with a shared bearer token from a file in `keys/`.
Semantics are at-least-once: unacknowledged entries redeliver after a lease
timeout, and the poll side's duplicate suppression absorbs the repeats.
Count-only notifications coalesce per device to the newest badge state.
Event content never crosses the relay (it is stripped before queueing, like
it is stripped from every push). Pushkeys a push service declares invalid
are reported back with the ack; the queue side answers `rejected` to the
homeserver's next notify for them — delayed by one round-trip, which is as
early as the push-gateway API allows.

The buffer is deliberately not persistent: pushes are wake-up signals
without content, clients resync on next open, so a restart merely costs a
wake-up. Put a TLS-terminating reverse proxy in front of the queue side; the
poll side speaks HTTPS natively.

[`docs/queue-poll-relay.md`](docs/queue-poll-relay.md) walks through a
complete deployment, from token generation to verification;
[`pincerbell-queue.toml.example`](pincerbell-queue.toml.example) and
[`pincerbell-poll.toml.example`](pincerbell-poll.toml.example) are
ready-to-copy configurations for the two roles.

## Running

```sh
cp pincerbell.toml.example pincerbell.toml   # edit: listen address, apps
cargo run
```

Three example configurations cover the deployment modes:
[`pincerbell.toml.example`](pincerbell.toml.example) for direct operation,
[`pincerbell-queue.toml.example`](pincerbell-queue.toml.example) and
[`pincerbell-poll.toml.example`](pincerbell-poll.toml.example) for the two
halves of the [queue/poll relay](#queuepoll-relay) — each gets copied to
`pincerbell.toml` on its host.

The config file lists the apps the gateway delivers for, one
`[apps."<app_id>"]` table each. Notifications for unconfigured app_ids are
logged and skipped by default; set `reject_unknown_apps = true` to reject
their pushkeys instead (which makes homeservers delete those pushers, so
enable it only once the app list is complete). In queue mode (`[queue]`
configured) unconfigured app_ids are queued for the poll side instead, and
`reject_unknown_apps` has no effect — only the poll side knows the real app
list. Message content is never written to the log — metadata only.

## Docker

A multi-stage [`Dockerfile`](Dockerfile) builds a static musl binary into a
bare Alpine image (TLS roots are compiled in, so the runtime stage needs no
extra packages). Configuration is a mounted TOML — it **must** set
`listen = "0.0.0.0:8300"`, since the built-in localhost default is
unreachable from outside the container.

Credential files (APNs `.p8` signing keys, FCM service-account JSONs) go
into the [`keys/`](keys/README.md) service directory, which compose mounts
read-only at `/etc/pincerbell/keys` — config entries reference them as
`/etc/pincerbell/keys/<file>`. Everything in `keys/` is gitignored and
excluded from the image; credentials reach the container only through the
volume mount.

[`compose.yml.example`](compose.yml.example) shows the setup: copy it to
`compose.yaml` (gitignored, like `pincerbell.toml` — real settings never
land in the repo), join the homeserver's Docker network, and point the
pusher URL at `http://pincerbell:8300/_matrix/push/v1/notify`.

```sh
cp compose.yml.example compose.yaml
cp pincerbell.toml.example pincerbell.toml   # set listen, add your apps
cp ~/Downloads/AuthKey_ABC123DEFG.p8 keys/   # whatever your apps reference
docker compose up --build
```

## License

MIT — see [LICENSE](LICENSE).
