//! Wire types for the Push Gateway API.
//!
//! These mirror the request/response schemas of `POST /_matrix/push/v1/notify`
//! as defined in the Matrix specification:
//! <https://spec.matrix.org/latest/push-gateway-api/>
//!
//! Unknown fields are tolerated (the spec may grow), absent optional fields
//! deserialize to `None`. `content` and `tweaks` are kept as raw JSON: their
//! shape is event- respectively push-rule-defined, not fixed by the gateway.
//!
//! The notification types also serialize (for the queue/poll relay, which
//! forwards them between pincerbell instances) -- except `content`, which
//! never leaves the gateway in any direction; absent optional fields stay
//! absent on the wire.

// The structs mirror the spec's schema in full; fields no delivery backend
// reads yet (pushkey_ts, tweaks, ...) are still part of the wire contract.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Request body of `POST /_matrix/push/v1/notify`.
#[derive(Debug, Deserialize)]
pub struct NotifyRequest {
    pub notification: Notification,
}

/// The notification object. All fields except `devices` are optional; with
/// the `event_id_only` format only `event_id`, `room_id`, `counts` and
/// `devices` are populated.
#[derive(Debug, Serialize, Deserialize)]
pub struct Notification {
    pub devices: Vec<Device>,
    /// Event content. NEVER serialized: it must not reach push services and
    /// must not cross the queue/poll relay either.
    #[serde(default, skip_serializing)]
    pub content: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counts: Option<Counts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_id: Option<String>,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub event_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_is_target: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prio: Option<Prio>,
}

impl Notification {
    /// Effective priority; the spec defaults an absent `prio` to high.
    pub fn priority(&self) -> Prio {
        self.prio.unwrap_or(Prio::High)
    }
}

/// One target device of a notification.
#[derive(Debug, Serialize, Deserialize)]
pub struct Device {
    pub app_id: String,
    pub pushkey: String,
    /// Unix timestamp (seconds) of the pushkey's last update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushkey_ts: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<PusherData>,
    /// Push-rule tweaks (sound, highlight, ...); shape is rule-defined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tweaks: Option<Map<String, Value>>,
}

/// Pusher data as set on the homeserver, minus its `url` key.
#[derive(Debug, Serialize, Deserialize)]
pub struct PusherData {
    /// Notification format requested by the client, e.g. `event_id_only`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

/// Unacknowledged-communication counts. Zero-valued counts are omitted on
/// the wire, so absent means zero.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Counts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unread: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missed_calls: Option<u64>,
}

/// Notification priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Prio {
    High,
    Low,
}

/// Response body: the pushkeys from the request that are not valid. The
/// homeserver stops notifying these and removes the associated pushers.
#[derive(Debug, Serialize)]
pub struct NotifyResponse {
    pub rejected: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_notification_deserializes() {
        let req: NotifyRequest = serde_json::from_value(serde_json::json!({
            "notification": {
                "event_id": "$event1:example.test",
                "room_id": "!room:example.test",
                "type": "m.room.message",
                "sender": "@alice:example.test",
                "sender_display_name": "Alice",
                "room_name": "Test Room",
                "room_alias": "#test:example.test",
                "user_is_target": false,
                "prio": "low",
                "content": { "msgtype": "m.text", "body": "hello" },
                "counts": { "unread": 2, "missed_calls": 1 },
                "devices": [{
                    "app_id": "org.example.app",
                    "pushkey": "pushkey-1",
                    "pushkey_ts": 1_700_000_000,
                    "data": { "format": "event_id_only", "extra": "kept" },
                    "tweaks": { "sound": "default" }
                }]
            }
        }))
        .unwrap();

        let n = req.notification;
        assert_eq!(n.event_id.as_deref(), Some("$event1:example.test"));
        assert_eq!(n.event_type.as_deref(), Some("m.room.message"));
        assert_eq!(n.priority(), Prio::Low);
        assert_eq!(n.counts.as_ref().unwrap().unread, Some(2));
        let d = &n.devices[0];
        assert_eq!(d.app_id, "org.example.app");
        assert_eq!(d.pushkey, "pushkey-1");
        let data = d.data.as_ref().unwrap();
        assert_eq!(data.format.as_deref(), Some("event_id_only"));
        assert_eq!(data.rest["extra"], "kept");
    }

    #[test]
    fn minimal_event_id_only_payload_deserializes() {
        let req: NotifyRequest = serde_json::from_value(serde_json::json!({
            "notification": {
                "event_id": "$event2:example.test",
                "room_id": "!room:example.test",
                "counts": { "unread": 1 },
                "devices": [{ "app_id": "org.example.app", "pushkey": "pushkey-2" }]
            }
        }))
        .unwrap();

        let n = req.notification;
        assert_eq!(n.priority(), Prio::High, "absent prio defaults to high");
        assert!(n.content.is_none());
        assert!(n.devices[0].tweaks.is_none());
    }

    #[test]
    fn serialization_strips_content_and_absent_fields() {
        let req: NotifyRequest = serde_json::from_value(serde_json::json!({
            "notification": {
                "event_id": "$event1:example.test",
                "room_id": "!room:example.test",
                "content": { "msgtype": "m.text", "body": "secret" },
                "counts": { "unread": 2 },
                "devices": [{
                    "app_id": "org.example.app",
                    "pushkey": "pushkey-1",
                    "data": { "format": "event_id_only", "extra": "kept" },
                }]
            }
        }))
        .unwrap();

        let v = serde_json::to_value(&req.notification).unwrap();
        assert!(v.get("content").is_none(), "content must never serialize");
        assert!(v.get("sender").is_none(), "absent fields stay absent");
        assert_eq!(v["event_id"], "$event1:example.test");
        assert_eq!(v["counts"]["unread"], 2);
        assert!(v["counts"].get("missed_calls").is_none());
        assert_eq!(v["devices"][0]["pushkey"], "pushkey-1");
        assert_eq!(v["devices"][0]["data"]["extra"], "kept");
        assert!(v["devices"][0].get("tweaks").is_none());

        // ... and what serialized still deserializes (relay round-trip).
        let n: Notification = serde_json::from_value(v).unwrap();
        assert!(n.content.is_none());
        assert_eq!(n.devices[0].app_id, "org.example.app");
    }

    #[test]
    fn devices_is_required() {
        let err = serde_json::from_value::<NotifyRequest>(serde_json::json!({
            "notification": { "event_id": "$e:example.test" }
        }));
        assert!(err.is_err());
    }
}
