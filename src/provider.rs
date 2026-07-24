//! Delivery backends and the outcome vocabulary the gateway acts on.

use crate::api::{Device, Notification};
use crate::apns::{ApnsApp, ApnsSettings};
use crate::config::AppConfig;
use crate::fcm::FcmApp;

/// One constructed delivery backend per configured app.
pub enum Provider {
    /// Logs notification metadata (never content); development/testing sink.
    Log,
    /// Firebase Cloud Messaging, HTTP v1 API. Boxed: the backend state
    /// dwarfs the Log variant.
    Fcm(Box<FcmApp>),
    /// Apple Push Notification service, HTTP/2 provider API.
    Apns(Box<ApnsApp>),
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
    pub fn new(config: &AppConfig) -> Result<Provider, String> {
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
            })?))),
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
        }
    }
}
