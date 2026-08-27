# Queue/poll relay: a worked example

This walks through a complete split deployment. The scenario: the
homeserver runs at a provider whose network cannot reach the push services
(FCM, APNs, ...), so notifications are buffered there and fetched by a
second pincerbell on a network that can deliver them.

```
provider network                          delivering network
┌──────────┐   /notify   ┌────────────┐                ┌────────────┐
│ Synapse  ├────────────►│ pincerbell │   HTTPS        │ pincerbell │──► FCM
│          │◄────────────┤ (queue)    │◄─── long-poll ─┤ (poll)     │──► APNs
└──────────┘  rejected   └────────────┘   poll + ack   └────────────┘──► ...
```

Both roles are the same binary; only the config differs. The poll side
opens the connection — outbound only — so it needs no inbound port, and the
queue side needs no route to any push service.

## 1. Shared token

Both sides authenticate with the same bearer token, read from a file in the
`keys/` service directory:

```sh
openssl rand -base64 32 > keys/queue-token
```

Copy the file to both hosts. Whitespace is trimmed, so a trailing newline
is fine.

## 2. Queue side (next to the homeserver)

Copy [`pincerbell-queue.toml.example`](../pincerbell-queue.toml.example) to
`pincerbell.toml`:

```toml
listen = "0.0.0.0:8300"

[queue]
auth_token_file = "/etc/pincerbell/keys/queue-token"
```

That is the whole file. No `[apps]` tables: with `[queue]` configured,
every app_id without an explicit entry is queued — the app list lives on
the poll side only. (An explicit `[apps]` entry would still deliver
directly, for mixed operation.)

Homeserver and client apps notice nothing: pushers point at the queue
side's `/_matrix/push/v1/notify` exactly as they would at a normal gateway.

### TLS proxy

Put a TLS-terminating reverse proxy in front of the queue side and make
sure its read timeout exceeds the long-poll hold (up to 60 s), e.g. nginx:

```nginx
location /_pincerbell/ {
    proxy_pass http://127.0.0.1:8300;
    proxy_read_timeout 90s;   # > the 60 s long-poll cap
}
location /_matrix/push/ {
    proxy_pass http://127.0.0.1:8300;
}
```

Only the homeserver needs `/_matrix/push/`; only the poll side needs
`/_pincerbell/` — restrict each further if your setup allows.

## 3. Poll side (on the delivering network)

Copy [`pincerbell-poll.toml.example`](../pincerbell-poll.toml.example) to
`pincerbell.toml`:

```toml
[[poll]]
url = "https://push-edge.example.org"
auth_token_file = "/etc/pincerbell/keys/queue-token"

[apps."org.example.androidapp"]
kind = "fcm"
service_account_file = "/etc/pincerbell/keys/service-account.json"

[apps."org.example.iosapp"]
kind = "apns"
key_file = "/etc/pincerbell/keys/AuthKey_ABC123DEFG.p8"
key_id   = "ABC123DEFG"
team_id  = "TEAM123456"
topic    = "org.example.iosapp"
```

The `[apps]` tables are exactly the ones a direct deployment would have
(all kinds and options in
[`pincerbell.toml.example`](../pincerbell.toml.example)). More `[[poll]]`
tables poll more queue sides; each upstream gets its own loop with
reconnect backoff.

Networks that only reach out through forward proxies are covered too — a
top-level `proxy` routes delivery to the push services, and each `[[poll]]`
entry takes its own `proxy` when the queue side sits behind a different
one (an empty string there forces a direct connection):

```toml
proxy = "http://internet-proxy.internal:3128"

[[poll]]
url = "https://push-edge.example.org"
auth_token_file = "/etc/pincerbell/keys/queue-token"
proxy = "http://queue-proxy.internal:3128"
```

## 4. Verify

Start both sides and send a test notification to the queue side:

```sh
curl -s https://push-edge.example.org/_matrix/push/v1/notify \
  -H 'content-type: application/json' \
  -d '{"notification": {
        "event_id": "$test:example.org",
        "room_id": "!test:example.org",
        "counts": {"unread": 1},
        "devices": [{"app_id": "org.example.androidapp", "pushkey": "test"}]}}'
```

The queue side logs `queued for poll side`, and within moments the poll
side logs the delivery attempt (here: an FCM error, since "test" is no real
registration token). The poll side's startup log shows one
`polling upstream queue` line per upstream; `/health` answers `ok` on both
sides.

## What to expect operationally

- **Delivery is at-least-once.** Unacknowledged entries redeliver after
  `lease_secs`; the poll side's duplicate suppression absorbs the repeats.
- **A poll-side outage is invisible to the homeserver.** The queue buffers
  (by default up to 262 144 metadata-only entries, entries older than
  `entry_ttl_secs` are dropped) and drains within seconds of the poll side
  reconnecting.
- **Invalid pushkeys are reported one attempt late.** The push service's
  verdict reaches the queue side with the acknowledgement, and the
  homeserver sees `rejected` on its *next* notify for that pushkey — the
  earliest the push-gateway API allows.
- **A queue-side restart loses the buffer.** That is deliberate: pushes
  are contentless wake-up signals and clients resync on next open, so the
  loss costs a wake-up, not data.
