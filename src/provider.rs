//! Delivery backends and the outcome vocabulary the gateway acts on.

use std::time::Duration;

use crate::api::{Device, Notification};
use crate::apns::{ApnsApp, ApnsSettings};
use crate::config::AppConfig;
use crate::fcm::FcmApp;
use crate::webpush::{WebPushApp, WebPushSettings};

/// Builds the reqwest client every outbound path uses, honoring the
/// resolved proxy/TLS options. Empty strings opt out (a `[[poll]]` entry
/// drops an inherited top-level `proxy`/`tls_ca_file` that way). Malformed
/// proxy URLs and unreadable CA bundles fail here, at startup, like the
/// credential files.
pub fn http_client(
    opts: &crate::config::HttpOptions,
    timeout: Option<Duration>,
) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder();
    if let Some(t) = timeout {
        builder = builder.timeout(t);
    }
    if let Some(url) = opts.proxy.as_deref().filter(|p| !p.is_empty()) {
        let proxy = reqwest::Proxy::all(url).map_err(|e| format!("proxy {url}: {e}"))?;
        builder = builder.proxy(proxy);
    }
    if let Some(path) = opts
        .tls_ca_file
        .as_deref()
        .filter(|p| !p.as_os_str().is_empty())
    {
        let pem =
            std::fs::read(path).map_err(|e| format!("tls_ca_file {}: {e}", path.display()))?;
        let certs = reqwest::Certificate::from_pem_bundle(&pem)
            .map_err(|e| format!("tls_ca_file {}: {e}", path.display()))?;
        if certs.is_empty() {
            return Err(format!(
                "tls_ca_file {}: no certificates found",
                path.display()
            ));
        }
        for cert in certs {
            builder = builder.add_root_certificate(cert);
        }
    }
    if opts.tls_accept_invalid_certs {
        tracing::warn!(
            "TLS certificate verification is DISABLED (tls_accept_invalid_certs) -- \
             anyone on the network path can impersonate the peers"
        );
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder.build().map_err(|e| e.to_string())
}

/// One constructed delivery backend per configured app.
pub enum Provider {
    /// Logs notification metadata (never content); development/testing sink.
    Log,
    /// Firebase Cloud Messaging, HTTP v1 API. Boxed: the backend state
    /// dwarfs the Log variant.
    Fcm(Box<FcmApp>),
    /// Apple Push Notification service, HTTP/2 provider API.
    Apns(Box<ApnsApp>),
    /// Web Push (RFC 8030/8291/8292) with VAPID.
    Webpush(Box<WebPushApp>),
}

/// What became of one device's delivery.
pub enum Outcome {
    /// Handed to the push service; recorded for duplicate suppression.
    Delivered,
    /// The pushkey is permanently invalid -- reported to the homeserver,
    /// which then deletes the pusher.
    Rejected,
    /// Permanent failure that is NOT the pushkey's fault (potentially our
    /// own bug); logged and dropped without rejecting.
    Skipped,
    /// Temporary failure; the gateway answers 502 so the homeserver retries.
    Transient(String),
}

impl Provider {
    pub fn new(config: &AppConfig, http: &crate::config::HttpOptions) -> Result<Provider, String> {
        match config {
            AppConfig::Log => Ok(Provider::Log),
            AppConfig::Fcm {
                service_account_file,
                project_id,
                api_root,
            } => Ok(Provider::Fcm(Box::new(FcmApp::new(
                service_account_file,
                project_id.clone(),
                api_root.clone(),
                http,
            )?))),
            AppConfig::Apns {
                key_file,
                key_id,
                team_id,
                topic,
                sandbox,
                api_root,
                default_alert_title,
                sound,
                push_type,
            } => Ok(Provider::Apns(Box::new(ApnsApp::new(ApnsSettings {
                key_file: key_file.clone(),
                key_id: key_id.clone(),
                team_id: team_id.clone(),
                topic: topic.clone(),
                sandbox: *sandbox,
                api_root: api_root.clone(),
                default_alert_title: default_alert_title.clone(),
                sound: sound.clone(),
                push_type: *push_type,
                http: http.clone(),
            })?))),
            AppConfig::Webpush {
                vapid_private_key,
                vapid_contact_email,
                allowed_endpoints,
            } => Ok(Provider::Webpush(Box::new(WebPushApp::new(
                WebPushSettings {
                    vapid_private_key: vapid_private_key.clone(),
                    vapid_contact_email: vapid_contact_email.clone(),
                    allowed_endpoints: allowed_endpoints.clone(),
                    http: http.clone(),
                },
            )?))),
        }
    }

    pub async fn deliver(&self, n: &Notification, device: &Device) -> Outcome {
        match self {
            Provider::Log => {
                tracing::info!(
                    app_id = %device.app_id,
                    pushkey = %device.pushkey,
                    event_id = n.event_id.as_deref().unwrap_or("-"),
                    room_id = n.room_id.as_deref().unwrap_or("-"),
                    event_type = n.event_type.as_deref().unwrap_or("-"),
                    prio = ?n.priority(),
                    unread = n.counts.as_ref().and_then(|c| c.unread),
                    "log sink: notification delivered"
                );
                Outcome::Delivered
            }
            Provider::Fcm(app) => app.deliver(n, device).await,
            Provider::Apns(app) => app.deliver(n, device).await,
            Provider::Webpush(app) => app.deliver(n, device).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HttpOptions;

    #[test]
    fn accept_invalid_certs_and_empty_optouts_build() {
        http_client(
            &HttpOptions {
                proxy: Some(String::new()),
                tls_ca_file: Some(std::path::PathBuf::new()),
                tls_accept_invalid_certs: true,
            },
            Some(Duration::from_secs(1)),
        )
        .unwrap();
    }

    #[test]
    fn malformed_ca_bundle_fails_at_startup() {
        let path = std::env::temp_dir().join(format!(
            "pincerbell-test-ca-{}-{:?}.pem",
            std::process::id(),
            std::thread::current().id(),
        ));
        std::fs::write(&path, "not a pem bundle").unwrap();
        let err = http_client(
            &HttpOptions {
                tls_ca_file: Some(path.clone()),
                ..Default::default()
            },
            None,
        )
        .unwrap_err();
        std::fs::remove_file(&path).ok();
        assert!(err.contains("tls_ca_file"), "{err}");
    }

    #[test]
    fn missing_ca_file_fails_at_startup() {
        let err = http_client(
            &HttpOptions {
                tls_ca_file: Some("/nonexistent/ca.pem".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap_err();
        assert!(err.contains("tls_ca_file"), "{err}");
    }
}
