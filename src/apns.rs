//! Delivery via the Apple Push Notification service (HTTP/2 provider API).
//!
//! Implemented against Apple's public documentation: notifications are sent
//! as `POST {api_root}/3/device/{device_token}` over HTTP/2, authorized with
//! token-based authentication -- an ES256-signed JWT (the `kid` header names
//! the Apple key, `iss` the team) built from a .p8 signing key downloaded
//! from the developer account. Apple wants the token reused between 20 and
//! 60 minutes; pincerbell refreshes after 45.
//!
//! Like the FCM backend, pincerbell never forwards event content. The
//! payload carries an `aps` dictionary with `mutable-content: 1` plus a
//! configurable fallback alert, and the notification METADATA as custom
//! keys: the app's notification service extension fetches the event and
//! rewrites the alert before display; if it cannot, iOS shows the fallback.
//! Count-only notifications (no event_id) become badge-only updates without
//! an alert.

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Value, json};

use crate::api::{Device, Notification, Prio};
use crate::config::ApnsPushType;
use crate::provider::Outcome;

/// Apple: reuse a provider token for 20-60 minutes.
const TOKEN_MAX_AGE: Duration = Duration::from_secs(45 * 60);

pub struct ApnsApp {
    client: reqwest::Client,
    api_root: String,
    topic: String,
    team_id: String,
    key_id: String,
    default_alert_title: String,
    sound: Option<String>,
    push_type: ApnsPushType,
    signing_key: jsonwebtoken::EncodingKey,
    token: Mutex<Option<CachedToken>>,
}

struct CachedToken {
    bearer: String,
    issued: Instant,
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    iat: u64,
}

pub struct ApnsSettings {
    pub key_file: std::path::PathBuf,
    pub key_id: String,
    pub team_id: String,
    pub topic: String,
    pub sandbox: bool,
    pub api_root: Option<String>,
    pub default_alert_title: Option<String>,
    pub sound: Option<String>,
    pub push_type: ApnsPushType,
    pub http: crate::config::HttpOptions,
}

impl ApnsApp {
    /// Reads the .p8 signing key and prepares the client; fails fast at
    /// startup rather than on the first notification.
    pub fn new(settings: ApnsSettings) -> Result<ApnsApp, String> {
        let pem = std::fs::read_to_string(&settings.key_file)
            .map_err(|e| format!("{}: {e}", settings.key_file.display()))?;
        let signing_key = jsonwebtoken::EncodingKey::from_ec_pem(pem.as_bytes())
            .map_err(|e| format!("{}: {e}", settings.key_file.display()))?;
        let client = crate::provider::http_client(&settings.http, Some(Duration::from_secs(15)))?;
        let api_root = settings.api_root.unwrap_or_else(|| {
            if settings.sandbox {
                "https://api.sandbox.push.apple.com".to_owned()
            } else {
                "https://api.push.apple.com".to_owned()
            }
        });
        Ok(ApnsApp {
            client,
            api_root,
            topic: settings.topic,
            team_id: settings.team_id,
            key_id: settings.key_id,
            default_alert_title: settings
                .default_alert_title
                .unwrap_or_else(|| "New message".to_owned()),
            sound: settings.sound,
            push_type: settings.push_type,
            signing_key,
            token: Mutex::new(None),
        })
    }

    fn bearer_token(&self) -> Result<String, String> {
        if let Some(t) = self.token.lock().unwrap().as_ref()
            && t.issued.elapsed() < TOKEN_MAX_AGE
        {
            return Ok(t.bearer.clone());
        }
        let iat = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before 1970")
            .as_secs();
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
        header.kid = Some(self.key_id.clone());
        let claims = Claims {
            iss: &self.team_id,
            iat,
        };
        let bearer = jsonwebtoken::encode(&header, &claims, &self.signing_key)
            .map_err(|e| format!("signing provider token: {e}"))?;
        *self.token.lock().unwrap() = Some(CachedToken {
            bearer: bearer.clone(),
            issued: Instant::now(),
        });
        Ok(bearer)
    }

    pub async fn deliver(&self, n: &Notification, device: &Device) -> Outcome {
        let bearer = match self.bearer_token() {
            Ok(b) => b,
            Err(e) => return Outcome::Transient(e),
        };
        let url = format!("{}/3/device/{}", self.api_root, device.pushkey);
        let (push_type, priority) = match self.push_type {
            // VoIP pushes are always immediate -- a queued incoming call is
            // a missed call.
            ApnsPushType::Voip => ("voip", "10"),
            ApnsPushType::Alert => (
                "alert",
                match n.priority() {
                    Prio::High => "10",
                    Prio::Low => "5",
                },
            ),
        };
        let resp = self
            .client
            .post(url)
            .header("authorization", format!("bearer {bearer}"))
            .header("apns-topic", &self.topic)
            .header("apns-push-type", push_type)
            .header("apns-priority", priority)
            .json(&self.build_payload(n, device))
            .send()
            .await;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => return Outcome::Transient(format!("apns: {e}")),
        };
        let status = resp.status();
        if status.is_success() {
            return Outcome::Delivered;
        }
        let body = resp.text().await.unwrap_or_default();

        // Only "Unregistered" (410: the device token is gone for good)
        // justifies a rejection -- that makes the homeserver DELETE the
        // pusher. BadDeviceToken in particular is NOT rejected: it also
        // fires on a sandbox/production mismatch, i.e. OUR misconfiguration.
        if status == reqwest::StatusCode::GONE && body.contains("Unregistered") {
            return Outcome::Rejected;
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            // InvalidProviderToken / ExpiredProviderToken: discard the
            // cached token so the retry signs a fresh one.
            *self.token.lock().unwrap() = None;
            return Outcome::Transient(format!("apns: HTTP 403 {body}"));
        }
        match status.as_u16() {
            408 | 429 => Outcome::Transient(format!("apns: HTTP {status}")),
            s if status.is_server_error() => Outcome::Transient(format!("apns: HTTP {s}")),
            _ => {
                tracing::error!(
                    app_id = %device.app_id,
                    status = %status,
                    body = %body.chars().take(500).collect::<String>(),
                    "apns: permanent delivery failure, skipping (not rejecting)"
                );
                Outcome::Skipped
            }
        }
    }

    /// Event notifications get a rewritable fallback alert
    /// (`mutable-content`), count-only ones just update the badge. VoIP
    /// pushes carry no alert/badge at all -- the app's CallKit integration
    /// handles the UI. The notification metadata rides as custom keys next
    /// to `aps` either way.
    fn build_payload(&self, n: &Notification, device: &Device) -> Value {
        let mut aps = serde_json::Map::new();
        let unread = n.counts.as_ref().and_then(|c| c.unread);
        if self.push_type == ApnsPushType::Alert
            && let Some(u) = unread
        {
            aps.insert("badge".to_owned(), json!(u));
        }
        if self.push_type == ApnsPushType::Alert && n.event_id.is_some() {
            aps.insert("mutable-content".to_owned(), json!(1));
            aps.insert(
                "alert".to_owned(),
                json!({ "title": self.default_alert_title }),
            );
            // The push-rule sound tweak wins over the configured default.
            let tweak_sound = device
                .tweaks
                .as_ref()
                .and_then(|t| t.get("sound"))
                .and_then(|v| v.as_str());
            if let Some(sound) = tweak_sound.or(self.sound.as_deref()) {
                aps.insert("sound".to_owned(), json!(sound));
            }
        }

        let mut payload = serde_json::Map::new();
        payload.insert("aps".to_owned(), Value::Object(aps));
        let mut put = |key: &str, value: Option<Value>| {
            if let Some(v) = value {
                payload.insert(key.to_owned(), v);
            }
        };
        put("event_id", n.event_id.clone().map(Value::String));
        put("room_id", n.room_id.clone().map(Value::String));
        put("type", n.event_type.clone().map(Value::String));
        put("sender", n.sender.clone().map(Value::String));
        put(
            "sender_display_name",
            n.sender_display_name.clone().map(Value::String),
        );
        put("room_name", n.room_name.clone().map(Value::String));
        put("unread", unread.map(|u| json!(u)));
        put(
            "missed_calls",
            n.counts
                .as_ref()
                .and_then(|c| c.missed_calls)
                .map(|m| json!(m)),
        );
        Value::Object(payload)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Throwaway EC P-256 key generated FOR THESE TESTS ONLY (openssl);
    /// it authenticates nothing anywhere.
    pub(crate) const TEST_EC_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg1RQOjcqCS7b0pmA2
nyE8g0es+6V9NOP7j3xz0FzBN8+hRANCAAQWQmKTghF7aFUSjmgmcbRG0xCM9O5w
6S+jCw94NhhS01iXjlO9uhaXbFCDE/Nb5jgST7pNQin3Uh0huOkMF+yW
-----END PRIVATE KEY-----
";

    fn test_app_with(push_type: ApnsPushType) -> ApnsApp {
        let key_path = std::env::temp_dir().join(format!(
            "pincerbell-test-apns-{}-{:?}.p8",
            std::process::id(),
            std::thread::current().id(),
        ));
        std::fs::write(&key_path, TEST_EC_KEY).unwrap();
        let app = ApnsApp::new(ApnsSettings {
            key_file: key_path.clone(),
            key_id: "TESTKEY123".into(),
            team_id: "TESTTEAM12".into(),
            topic: "org.example.iosapp".into(),
            sandbox: false,
            api_root: None,
            default_alert_title: None,
            sound: None,
            push_type,
            http: Default::default(),
        })
        .unwrap();
        std::fs::remove_file(key_path).ok();
        app
    }

    fn test_app() -> ApnsApp {
        test_app_with(ApnsPushType::Alert)
    }

    fn notification(json: Value) -> Notification {
        serde_json::from_value::<crate::api::NotifyRequest>(json!({"notification": json}))
            .unwrap()
            .notification
    }

    #[test]
    fn event_payload_has_rewritable_alert_and_metadata() {
        let app = test_app();
        let n = notification(json!({
            "event_id": "$e1:example.test",
            "room_id": "!r:example.test",
            "sender": "@alice:example.test",
            "counts": { "unread": 4 },
            "content": { "body": "must not be forwarded" },
            "devices": [{
                "app_id": "org.example.iosapp",
                "pushkey": "device-token-1",
                "tweaks": { "sound": "ping" },
            }]
        }));
        let p = app.build_payload(&n, &n.devices[0]);

        assert_eq!(p["aps"]["mutable-content"], 1);
        assert_eq!(p["aps"]["alert"]["title"], "New message");
        assert_eq!(p["aps"]["badge"], 4);
        assert_eq!(p["aps"]["sound"], "ping"); // tweak wins
        assert_eq!(p["event_id"], "$e1:example.test");
        assert_eq!(p["unread"], 4);
        // Event content stays out of the push payload.
        assert!(p.get("content").is_none());
    }

    #[test]
    fn voip_payload_has_no_alert_or_badge() {
        let app = test_app_with(ApnsPushType::Voip);
        let n = notification(json!({
            "event_id": "$call1:example.test",
            "room_id": "!r:example.test",
            "type": "m.call.invite",
            "counts": { "unread": 4 },
            "devices": [{ "app_id": "org.example.iosapp.voip", "pushkey": "voip-token-1" }]
        }));
        let p = app.build_payload(&n, &n.devices[0]);

        assert_eq!(p["aps"], json!({}));
        // The metadata still rides along for the CallKit handler.
        assert_eq!(p["event_id"], "$call1:example.test");
        assert_eq!(p["type"], "m.call.invite");
    }

    #[test]
    fn count_only_payload_is_badge_only() {
        let app = test_app();
        let n = notification(json!({
            "counts": { "unread": 2 },
            "devices": [{ "app_id": "org.example.iosapp", "pushkey": "device-token-2" }]
        }));
        let p = app.build_payload(&n, &n.devices[0]);

        assert_eq!(p["aps"]["badge"], 2);
        assert!(p["aps"].get("alert").is_none());
        assert!(p["aps"].get("mutable-content").is_none());
        assert!(p["aps"].get("sound").is_none());
    }

    #[test]
    fn provider_token_is_cached_and_reused() {
        let app = test_app();
        let t1 = app.bearer_token().unwrap();
        let t2 = app.bearer_token().unwrap();
        assert_eq!(t1, t2);
        assert_eq!(t1.split('.').count(), 3, "JWT has three segments");
    }
}
