//! Delivery via Firebase Cloud Messaging (HTTP v1 API).
//!
//! Implemented against Google's public documentation: messages are sent as
//! `POST {api_root}/v1/projects/{project_id}/messages:send`, authorized with
//! an OAuth2 bearer token obtained through the service-account JWT flow
//! (RS256-signed assertion posted to the account's `token_uri`, scope
//! `https://www.googleapis.com/auth/firebase.messaging`). Tokens are cached
//! until shortly before expiry.
//!
//! pincerbell sends data-only messages: the notification's METADATA
//! (event_id, room_id, type, sender, ...) as FCM `data` -- values must be
//! strings per FCM -- plus the Matrix priority mapped onto Android's. Event
//! content is deliberately not included: the client app fetches the event
//! itself, which keeps message bodies out of Google's push pipeline and the
//! payload safely under FCM's size limit.

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::api::{Device, Notification, Prio};
use crate::provider::Outcome;

const SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";
const TOKEN_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
/// Remaining validity below which a cached token is refreshed.
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// The subset of a Google service-account JSON file pincerbell needs.
#[derive(Deserialize)]
struct ServiceAccount {
    project_id: String,
    private_key: String,
    client_email: String,
    token_uri: String,
}

pub struct FcmApp {
    client: reqwest::Client,
    api_root: String,
    project_id: String,
    account: ServiceAccount,
    signing_key: jsonwebtoken::EncodingKey,
    token: Mutex<Option<CachedToken>>,
}

struct CachedToken {
    bearer: String,
    expires: Instant,
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    iat: u64,
    exp: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

impl FcmApp {
    /// Reads the service-account file and prepares the signing key; fails
    /// fast at startup rather than on the first notification.
    pub fn new(
        service_account_file: &std::path::Path,
        project_id: Option<String>,
        api_root: Option<String>,
    ) -> Result<FcmApp, String> {
        let raw = std::fs::read_to_string(service_account_file)
            .map_err(|e| format!("{}: {e}", service_account_file.display()))?;
        let account: ServiceAccount = serde_json::from_str(&raw)
            .map_err(|e| format!("{}: {e}", service_account_file.display()))?;
        let signing_key =
            jsonwebtoken::EncodingKey::from_rsa_pem(account.private_key.as_bytes())
                .map_err(|e| format!("{}: private_key: {e}", service_account_file.display()))?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(FcmApp {
            client,
            api_root: api_root.unwrap_or_else(|| "https://fcm.googleapis.com".to_owned()),
            project_id: project_id.unwrap_or_else(|| account.project_id.clone()),
            account,
            signing_key,
            token: Mutex::new(None),
        })
    }

    async fn access_token(&self) -> Result<String, String> {
        if let Some(t) = self.token.lock().unwrap().as_ref()
            && t.expires > Instant::now() + TOKEN_REFRESH_MARGIN
        {
            return Ok(t.bearer.clone());
        }

        let iat = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before 1970")
            .as_secs();
        let claims = Claims {
            iss: &self.account.client_email,
            scope: SCOPE,
            aud: &self.account.token_uri,
            iat,
            exp: iat + 3600,
        };
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        let assertion = jsonwebtoken::encode(&header, &claims, &self.signing_key)
            .map_err(|e| format!("signing token assertion: {e}"))?;

        let resp = self
            .client
            .post(&self.account.token_uri)
            .form(&[("grant_type", TOKEN_GRANT_TYPE), ("assertion", &assertion)])
            .send()
            .await
            .map_err(|e| format!("token endpoint: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("token endpoint: HTTP {}", resp.status()));
        }
        let token: TokenResponse = resp
            .json()
            .await
            .map_err(|e| format!("token endpoint: {e}"))?;

        let bearer = token.access_token;
        *self.token.lock().unwrap() = Some(CachedToken {
            bearer: bearer.clone(),
            expires: Instant::now() + Duration::from_secs(token.expires_in),
        });
        Ok(bearer)
    }

    pub async fn deliver(&self, n: &Notification, device: &Device) -> Outcome {
        let bearer = match self.access_token().await {
            Ok(b) => b,
            Err(e) => return Outcome::Transient(e),
        };
        let url = format!(
            "{}/v1/projects/{}/messages:send",
            self.api_root, self.project_id
        );
        let resp = self
            .client
            .post(url)
            .bearer_auth(bearer)
            .json(&build_message(n, device))
            .send()
            .await;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => return Outcome::Transient(format!("fcm: {e}")),
        };
        let status = resp.status();
        if status.is_success() {
            return Outcome::Delivered;
        }
        let body = resp.text().await.unwrap_or_default();

        // Only UNREGISTERED (the token is gone for good) justifies a
        // rejection -- that makes the homeserver DELETE the pusher. Anything
        // else 4xx could be our bug or misconfiguration and must not destroy
        // pushers; 401/403/429/5xx are treated as transient so the
        // homeserver retries.
        if status == reqwest::StatusCode::NOT_FOUND && body.contains("UNREGISTERED") {
            return Outcome::Rejected;
        }
        match status.as_u16() {
            401 | 403 | 408 | 429 => Outcome::Transient(format!("fcm: HTTP {status}")),
            s if status.is_server_error() => Outcome::Transient(format!("fcm: HTTP {s}")),
            _ => {
                tracing::error!(
                    app_id = %device.app_id,
                    status = %status,
                    body = %body.chars().take(500).collect::<String>(),
                    "fcm: permanent delivery failure, skipping (not rejecting)"
                );
                Outcome::Skipped
            }
        }
    }
}

/// Builds the FCM v1 message: pushkey = registration token, notification
/// metadata as string-valued `data`, Matrix priority mapped onto Android's.
fn build_message(n: &Notification, device: &Device) -> Value {
    let mut data = Map::new();
    let mut put = |key: &str, value: Option<String>| {
        if let Some(v) = value {
            data.insert(key.to_owned(), Value::String(v));
        }
    };
    put("event_id", n.event_id.clone());
    put("room_id", n.room_id.clone());
    put("type", n.event_type.clone());
    put("sender", n.sender.clone());
    put("sender_display_name", n.sender_display_name.clone());
    put("room_name", n.room_name.clone());
    let counts = n.counts.as_ref();
    put(
        "unread",
        counts.and_then(|c| c.unread).map(|u| u.to_string()),
    );
    put(
        "missed_calls",
        counts.and_then(|c| c.missed_calls).map(|m| m.to_string()),
    );

    json!({
        "message": {
            "token": device.pushkey,
            "data": data,
            "android": {
                "priority": match n.priority() {
                    Prio::High => "HIGH",
                    Prio::Low => "NORMAL",
                },
            },
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notification(json: Value) -> Notification {
        serde_json::from_value::<crate::api::NotifyRequest>(json!({"notification": json}))
            .unwrap()
            .notification
    }

    #[test]
    fn message_carries_metadata_as_string_data() {
        let n = notification(json!({
            "event_id": "$e1:example.test",
            "room_id": "!r:example.test",
            "type": "m.room.message",
            "sender": "@alice:example.test",
            "prio": "low",
            "counts": { "unread": 7 },
            "content": { "body": "must not be forwarded" },
            "devices": [{ "app_id": "org.example.app", "pushkey": "tok-1" }]
        }));
        let msg = build_message(&n, &n.devices[0]);

        assert_eq!(msg["message"]["token"], "tok-1");
        assert_eq!(msg["message"]["data"]["event_id"], "$e1:example.test");
        assert_eq!(msg["message"]["data"]["unread"], "7"); // string, per FCM
        assert_eq!(msg["message"]["android"]["priority"], "NORMAL");
        // Event content stays out of the push payload.
        assert!(msg["message"]["data"].get("content").is_none());
        assert!(msg["message"].get("notification").is_none());
    }

    #[test]
    fn minimal_notification_yields_minimal_data() {
        let n = notification(json!({
            "devices": [{ "app_id": "org.example.app", "pushkey": "tok-2" }]
        }));
        let msg = build_message(&n, &n.devices[0]);
        assert_eq!(msg["message"]["data"], json!({}));
        assert_eq!(msg["message"]["android"]["priority"], "HIGH");
    }
}
