# pincerbell

An independent Matrix Push Gateway implementation in Rust.

`pincerbell` implements the [Matrix Push Gateway API](https://spec.matrix.org/latest/push-gateway-api/)
so that Matrix homeservers can deliver push notifications to client apps.
It is written from scratch against the public specification and is not a
port of, based on, or affiliated with any other Push Gateway implementation
(such as Element's [Sygnal](https://github.com/element-hq/sygnal)) or with
the Matrix.org Foundation.

**Status:** early stage. The `POST /_matrix/push/v1/notify` endpoint is
implemented with duplicate suppression for homeserver retries and three
delivery backends: **FCM** (Firebase Cloud Messaging, HTTP v1 API, via a
Google service account), **APNs** (Apple Push Notification service, HTTP/2
provider API, token-based auth via a .p8 key), and a **log sink** for
development and testing.

Pushes never carry the event content — only notification metadata
(event_id, room_id, type, sender, ...); the client app fetches the event
itself. On FCM that means data-only messages; on APNs an alert with
`mutable-content: 1` whose fallback text the app's notification service
extension replaces after fetching the event (count-only notifications
become badge-only updates). A pushkey is only reported as rejected on FCM
`UNREGISTERED` / APNs `Unregistered`; transient errors fail the request
with 502 so the homeserver retries (already-delivered devices are shielded
by the duplicate suppression).

## Running

```sh
cp pincerbell.toml.example pincerbell.toml   # edit: listen address, apps
cargo run
```

The config file lists the apps the gateway delivers for, one
`[apps."<app_id>"]` table each. Notifications for unconfigured app_ids are
logged and skipped by default; set `reject_unknown_apps = true` to reject
their pushkeys instead (which makes homeservers delete those pushers, so
enable it only once the app list is complete). Message content is never
written to the log — metadata only.

## Docker

A multi-stage [`Dockerfile`](Dockerfile) builds a static musl binary into a
bare Alpine image (TLS roots are compiled in, so the runtime stage needs no
extra packages). Configuration is a mounted TOML — it **must** set
`listen = "0.0.0.0:8300"`, since the built-in localhost default is
unreachable from outside the container; FCM service-account / APNs .p8 key
files are mounted next to it.

[`compose.yml.example`](compose.yml.example) shows the setup: copy it to
`compose.yaml` (gitignored, like `pincerbell.toml` and any key files — real
credentials never land in the repo), join the homeserver's Docker network,
and point the pusher URL at `http://pincerbell:8300/_matrix/push/v1/notify`.

```sh
cp compose.yml.example compose.yaml
cp pincerbell.toml.example pincerbell.toml   # set listen, add your apps
docker compose up --build
```

## License

MIT — see [LICENSE](LICENSE).
