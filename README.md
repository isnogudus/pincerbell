# pincerbell

An independent Matrix Push Gateway implementation in Rust.

`pincerbell` implements the [Matrix Push Gateway API](https://spec.matrix.org/latest/push-gateway-api/)
so that Matrix homeservers can deliver push notifications to client apps.
It is written from scratch against the public specification and is not a
port of, based on, or affiliated with any other Push Gateway implementation
(such as Element's [Sygnal](https://github.com/element-hq/sygnal)) or with
the Matrix.org Foundation.

**Status:** early stage. The `POST /_matrix/push/v1/notify` endpoint is
implemented with duplicate suppression for homeserver retries and two
delivery backends: **FCM** (Firebase Cloud Messaging, HTTP v1 API, via a
Google service account) and a **log sink** for development and testing.
APNs does not exist yet.

FCM messages are data-only and carry notification metadata (event_id,
room_id, type, sender, ...) — never the event content; the client app
fetches the event itself. A pushkey is only reported as rejected when FCM
answers `UNREGISTERED`; transient errors fail the request with 502 so the
homeserver retries (already-delivered devices are shielded by the duplicate
suppression).

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

## License

MIT — see [LICENSE](LICENSE).
