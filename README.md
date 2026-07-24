# pincerbell

An independent Matrix Push Gateway implementation in Rust.

`pincerbell` implements the [Matrix Push Gateway API](https://spec.matrix.org/latest/push-gateway-api/)
so that Matrix homeservers can deliver push notifications to client apps.
It is written from scratch against the public specification and is not a
port of, based on, or affiliated with any other Push Gateway implementation
(such as Element's [Sygnal](https://github.com/element-hq/sygnal)) or with
the Matrix.org Foundation.

**Status:** early stage. The `POST /_matrix/push/v1/notify` endpoint is
implemented with a log-sink delivery backend for development and testing;
real push providers (APNs, FCM) do not exist yet.

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
