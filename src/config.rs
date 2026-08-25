//! TOML configuration: listen address plus one `[apps."<app_id>"]` table per
//! app this gateway is willing to deliver for.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// HTTP listen address. The default port 8300 deliberately avoids 5000,
    /// which macOS occupies with its AirPlay receiver.
    #[serde(default = "default_listen")]
    pub listen: String,

    /// How long a delivered (event_id, app_id, pushkey) triple suppresses
    /// repeat deliveries, in seconds -- covers homeserver retries after HTTP
    /// errors, which use bounded exponential backoff. 0 disables suppression.
    #[serde(default = "default_dedup_ttl_secs")]
    pub dedup_ttl_secs: u64,

    /// Memory cap for the suppression cache; oldest entries are evicted
    /// first once it is reached.
    #[serde(default = "default_dedup_max_entries")]
    pub dedup_max_entries: usize,

    /// Whether a notification for an app_id that is not configured rejects
    /// the pushkey (making the homeserver delete the pusher permanently).
    /// Off by default: a config typo must not destroy pushers at scale --
    /// unknown apps are logged and skipped instead. Turn on once the app
    /// list is known-complete.
    #[serde(default)]
    pub reject_unknown_apps: bool,

    /// Apps this gateway delivers for, keyed by app_id.
    #[serde(default)]
    pub apps: HashMap<String, AppConfig>,

    /// Queue mode: hold notifications for a remote poll-side instance to
    /// fetch over HTTPS long-poll, instead of delivering them here. When
    /// set, the queue becomes the fallback for every app_id without an
    /// explicit `[apps]` entry (explicit entries still deliver directly),
    /// and `reject_unknown_apps` has no effect -- only the poll side knows
    /// which apps exist.
    #[serde(default)]
    pub queue: Option<QueueConfig>,

    /// Poll mode: upstream queue-side instances this gateway long-polls for
    /// notifications, `[[poll]]` table each. Delivery then uses the `[apps]`
    /// entries configured here.
    #[serde(default)]
    pub poll: Vec<PollUpstream>,
}

/// The `[queue]` table: queue-side settings.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueConfig {
    /// File holding the shared bearer token the poll side authenticates
    /// with (whitespace-trimmed). Lives in the keys/ service directory like
    /// the other credentials.
    pub auth_token_file: std::path::PathBuf,

    /// Ring-buffer capacity; oldest entries are evicted first once reached.
    /// Entries are metadata-only (a few hundred bytes each), so the default
    /// tops out around 128 MB while buffering hours of a poll-side outage.
    #[serde(default = "default_queue_max_entries")]
    pub max_entries: usize,

    /// Entries older than this are dropped undelivered -- a push this stale
    /// is worthless, the client resyncs on next open anyway.
    #[serde(default = "default_queue_entry_ttl_secs")]
    pub entry_ttl_secs: u64,

    /// How long a polled entry stays leased before an unacknowledged
    /// delivery is handed out again (at-least-once).
    #[serde(default = "default_queue_lease_secs")]
    pub lease_secs: u64,

    /// How long a pushkey reported invalid by the poll side keeps answering
    /// `rejected` to the homeserver. After expiry the cycle simply repeats
    /// on the next delivery attempt. 0 disables the feedback.
    #[serde(default = "default_queue_rejected_ttl_secs")]
    pub rejected_ttl_secs: u64,
}

fn default_queue_max_entries() -> usize {
    262_144
}

fn default_queue_entry_ttl_secs() -> u64 {
    3600
}

fn default_queue_lease_secs() -> u64 {
    60
}

fn default_queue_rejected_ttl_secs() -> u64 {
    21_600
}

/// One `[[poll]]` table: an upstream queue-side instance to long-poll.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PollUpstream {
    /// Base URL of the queue-side instance, e.g. "https://push-edge.example.com".
    pub url: String,

    /// File holding the shared bearer token (whitespace-trimmed).
    pub auth_token_file: std::path::PathBuf,

    /// Long-poll hold time requested per request; the server caps at 60.
    #[serde(default = "default_poll_timeout_secs")]
    pub timeout_secs: u64,

    /// Maximum entries fetched per poll.
    #[serde(default = "default_poll_max_batch")]
    pub max_batch: usize,
}

fn default_poll_timeout_secs() -> u64 {
    30
}

fn default_poll_max_batch() -> usize {
    100
}

fn default_listen() -> String {
    "127.0.0.1:8300".to_owned()
}

fn default_dedup_ttl_secs() -> u64 {
    300
}

fn default_dedup_max_entries() -> usize {
    65_536
}

/// Per-app delivery backend.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum AppConfig {
    /// Writes notification metadata to the log instead of contacting a push
    /// service -- the development/testing sink.
    Log,
    /// Firebase Cloud Messaging (HTTP v1), authorized via a Google
    /// service-account JSON file.
    Fcm {
        service_account_file: std::path::PathBuf,
        /// Defaults to the service account's own project_id.
        #[serde(default)]
        project_id: Option<String>,
        /// API endpoint override; for tests and proxies. Defaults to
        /// https://fcm.googleapis.com.
        #[serde(default)]
        api_root: Option<String>,
    },
    /// Apple Push Notification service (HTTP/2 provider API), token-based
    /// authentication via a .p8 signing key.
    Apns {
        /// The .p8 key downloaded from the Apple developer account.
        key_file: std::path::PathBuf,
        /// Apple key ID belonging to the .p8 key.
        key_id: String,
        /// Apple developer team ID.
        team_id: String,
        /// The app's bundle ID (the apns-topic header).
        topic: String,
        /// Use the sandbox environment (api.sandbox.push.apple.com).
        #[serde(default)]
        sandbox: bool,
        /// API endpoint override; for tests and proxies. Defaults to the
        /// production or sandbox APNs endpoint depending on `sandbox`.
        #[serde(default)]
        api_root: Option<String>,
        /// Fallback alert title, shown only when the app's notification
        /// service extension cannot rewrite the notification in time.
        #[serde(default)]
        default_alert_title: Option<String>,
        /// Default notification sound; a push-rule sound tweak wins.
        #[serde(default)]
        sound: Option<String>,
        /// "alert" (default) or "voip". VoIP pushes target a PushKit VoIP
        /// device token, are always sent at full priority and carry no
        /// alert/badge -- the app's CallKit integration handles the UI. The
        /// topic must then be the app's VoIP topic (<bundle id>.voip).
        #[serde(default)]
        push_type: ApnsPushType,
    },
    /// Web Push (RFC 8030/8291/8292) for browser subscriptions, authorized
    /// via VAPID.
    Webpush {
        /// P-256 VAPID private key, PEM (PKCS#8 or SEC1).
        vapid_private_key: std::path::PathBuf,
        /// Contact address for the VAPID `sub` claim (sent as mailto:).
        vapid_contact_email: String,
        /// Mandatory allowlist of push-service hosts ("host" exact,
        /// "*.host" for subdomains). Subscription endpoints are
        /// client-controlled; without this gate the gateway would POST to
        /// arbitrary URLs (SSRF).
        allowed_endpoints: Vec<String>,
    },
}

/// The apns-push-type an APNs app sends with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApnsPushType {
    #[default]
    Alert,
    Voip,
}

impl Config {
    /// Loads the config file; a missing file yields the built-in defaults
    /// (local listen address, no apps) so the binary runs out of the box.
    pub fn load(path: &Path) -> Result<Config, String> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(path = %path.display(), "config file not found, using defaults");
                String::new()
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        toml::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_yields_defaults() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.listen, "127.0.0.1:8300");
        assert!(!c.reject_unknown_apps);
        assert!(c.apps.is_empty());
        assert_eq!(c.dedup_ttl_secs, 300);
        assert_eq!(c.dedup_max_entries, 65_536);
    }

    #[test]
    fn app_table_parses() {
        let c: Config = toml::from_str(
            r#"
            listen = "0.0.0.0:6000"
            reject_unknown_apps = true

            [apps."org.example.app"]
            kind = "log"
            "#,
        )
        .unwrap();
        assert_eq!(c.listen, "0.0.0.0:6000");
        assert!(c.reject_unknown_apps);
        assert!(matches!(c.apps["org.example.app"], AppConfig::Log));
    }

    #[test]
    fn fcm_app_table_parses() {
        let c: Config = toml::from_str(
            r#"
            [apps."org.example.app"]
            kind = "fcm"
            service_account_file = "/etc/pincerbell/sa.json"
            "#,
        )
        .unwrap();
        match &c.apps["org.example.app"] {
            AppConfig::Fcm {
                service_account_file,
                project_id,
                api_root,
            } => {
                assert_eq!(
                    service_account_file,
                    std::path::Path::new("/etc/pincerbell/sa.json")
                );
                assert!(project_id.is_none());
                assert!(api_root.is_none());
            }
            other => panic!("expected fcm, got {other:?}"),
        }
    }

    #[test]
    fn apns_app_table_parses() {
        let c: Config = toml::from_str(
            r#"
            [apps."org.example.iosapp"]
            kind = "apns"
            key_file = "/etc/pincerbell/AuthKey_TESTKEY123.p8"
            key_id = "TESTKEY123"
            team_id = "TESTTEAM12"
            topic = "org.example.iosapp"
            "#,
        )
        .unwrap();
        match &c.apps["org.example.iosapp"] {
            AppConfig::Apns {
                key_id,
                team_id,
                topic,
                sandbox,
                ..
            } => {
                assert_eq!(key_id, "TESTKEY123");
                assert_eq!(team_id, "TESTTEAM12");
                assert_eq!(topic, "org.example.iosapp");
                assert!(!sandbox);
            }
            other => panic!("expected apns, got {other:?}"),
        }
    }

    #[test]
    fn unknown_top_level_key_is_rejected() {
        assert!(toml::from_str::<Config>("lisen = \"oops\"").is_err());
    }

    #[test]
    fn example_config_files_parse() {
        for (name, check) in [
            (
                "pincerbell.toml.example",
                (|c: &Config| c.queue.is_none() && c.poll.is_empty()) as fn(&Config) -> bool,
            ),
            ("pincerbell-queue.toml.example", |c: &Config| {
                c.queue.is_some()
            }),
            ("pincerbell-poll.toml.example", |c: &Config| {
                !c.poll.is_empty() && !c.apps.is_empty()
            }),
        ] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
            let raw = std::fs::read_to_string(&path).unwrap();
            let c: Config =
                toml::from_str(&raw).unwrap_or_else(|e| panic!("{name} does not parse: {e}"));
            assert!(check(&c), "{name} does not configure its mode");
        }
    }

    #[test]
    fn queue_table_parses_with_defaults() {
        let c: Config = toml::from_str(
            r#"
            [queue]
            auth_token_file = "/etc/pincerbell/keys/queue-token"
            "#,
        )
        .unwrap();
        let q = c.queue.unwrap();
        assert_eq!(
            q.auth_token_file,
            std::path::Path::new("/etc/pincerbell/keys/queue-token")
        );
        assert_eq!(q.max_entries, 262_144);
        assert_eq!(q.entry_ttl_secs, 3600);
        assert_eq!(q.lease_secs, 60);
        assert_eq!(q.rejected_ttl_secs, 21_600);
        assert!(c.poll.is_empty());
    }

    #[test]
    fn multiple_poll_upstreams_parse() {
        let c: Config = toml::from_str(
            r#"
            [[poll]]
            url = "https://edge1.example.test"
            auth_token_file = "/etc/pincerbell/keys/edge1-token"

            [[poll]]
            url = "https://edge2.example.test"
            auth_token_file = "/etc/pincerbell/keys/edge2-token"
            timeout_secs = 45
            max_batch = 10
            "#,
        )
        .unwrap();
        assert!(c.queue.is_none());
        assert_eq!(c.poll.len(), 2);
        assert_eq!(c.poll[0].url, "https://edge1.example.test");
        assert_eq!(c.poll[0].timeout_secs, 30);
        assert_eq!(c.poll[0].max_batch, 100);
        assert_eq!(c.poll[1].timeout_secs, 45);
        assert_eq!(c.poll[1].max_batch, 10);
    }
}
