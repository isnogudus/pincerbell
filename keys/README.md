# Service key directory

Drop the credential files your `pincerbell.toml` references here — APNs
`.p8` signing keys, FCM service-account JSONs, and similar. The compose
setup mounts this directory read-only at `/etc/pincerbell/keys`, so a config
entry looks like:

```toml
[apps."org.example.iosapp"]
kind = "apns"
key_file = "/etc/pincerbell/keys/AuthKey_ABC123DEFG.p8"
# ...
```

Everything in this directory except this README is **gitignored** (and
excluded from the Docker build context): credentials must never end up in
the repository or inside the image — they reach the container only through
the volume mount.
