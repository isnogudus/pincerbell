//! The notify endpoint: fan a notification out to its target devices'
//! configured delivery backends and report invalid pushkeys back.

use std::sync::Arc;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::api::{Device, Notification, NotifyRequest, NotifyResponse};
use crate::config::{AppConfig, Config};

pub struct AppState {
    pub config: Config,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/_matrix/push/v1/notify", post(notify))
        .route("/health", get(|| async { "ok" }))
        .with_state(state)
}

/// `POST /_matrix/push/v1/notify`
///
/// Always answers 200 with the list of rejected pushkeys; the homeserver
/// deletes the pushers behind those keys, so only permanently undeliverable
/// keys belong in it -- transient delivery failures do not.
async fn notify(
    State(state): State<Arc<AppState>>,
    Json(req): Json<NotifyRequest>,
) -> Json<NotifyResponse> {
    let n = &req.notification;
    let mut rejected = Vec::new();

    for device in &n.devices {
        match state.config.apps.get(&device.app_id) {
            Some(app) => deliver(app, n, device),
            None if state.config.reject_unknown_apps => {
                tracing::info!(app_id = %device.app_id, "unknown app_id, rejecting pushkey");
                rejected.push(device.pushkey.clone());
            }
            None => {
                tracing::warn!(app_id = %device.app_id, "unknown app_id, skipping device");
            }
        }
    }

    Json(NotifyResponse { rejected })
}

/// Hands one device's notification to its app's backend. Logs metadata only,
/// never `content` -- message bodies must not end up in log files.
fn deliver(app: &AppConfig, n: &Notification, device: &Device) {
    match app {
        AppConfig::Log => {
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use tower::util::ServiceExt;

    fn test_router(toml: &str) -> Router {
        let config: Config = ::toml::from_str(toml).unwrap();
        router(Arc::new(AppState { config }))
    }

    async fn notify_call(
        router: Router,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method("POST")
            .uri("/_matrix/push/v1/notify")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    fn notification(app_id: &str, pushkey: &str) -> serde_json::Value {
        serde_json::json!({
            "notification": {
                "event_id": "$event1:example.test",
                "room_id": "!room:example.test",
                "counts": { "unread": 1 },
                "devices": [{ "app_id": app_id, "pushkey": pushkey }]
            }
        })
    }

    #[tokio::test]
    async fn known_app_is_accepted() {
        let r = test_router("[apps.\"org.example.app\"]\nkind = \"log\"");
        let (status, json) = notify_call(r, notification("org.example.app", "pk-1")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["rejected"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn unknown_app_is_skipped_by_default() {
        let r = test_router("");
        let (status, json) = notify_call(r, notification("org.example.unknown", "pk-2")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["rejected"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn unknown_app_is_rejected_when_configured() {
        let r = test_router("reject_unknown_apps = true");
        let (status, json) = notify_call(r, notification("org.example.unknown", "pk-3")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["rejected"], serde_json::json!(["pk-3"]));
    }

    #[tokio::test]
    async fn malformed_body_is_a_client_error() {
        let r = test_router("");
        let req = Request::builder()
            .method("POST")
            .uri("/_matrix/push/v1/notify")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{ not json"))
            .unwrap();
        let resp = r.oneshot(req).await.unwrap();
        assert!(resp.status().is_client_error());
    }

    #[tokio::test]
    async fn health_endpoint_answers() {
        let r = test_router("");
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = r.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
