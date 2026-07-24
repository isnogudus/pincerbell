//! The notify endpoint: fan a notification out to its target devices'
//! configured delivery backends and report invalid pushkeys back.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::api::{NotifyRequest, NotifyResponse};
use crate::config::Config;
use crate::dedup::DedupCache;
use crate::provider::{Outcome, Provider};

pub struct AppState {
    pub config: Config,
    pub providers: HashMap<String, Provider>,
    pub dedup: DedupCache,
}

impl AppState {
    /// Builds the delivery backends from the config; fails fast on a broken
    /// app entry (unreadable service-account file etc.).
    pub fn new(config: Config) -> Result<Self, String> {
        let mut providers = HashMap::new();
        for (app_id, app) in &config.apps {
            let provider = Provider::new(app).map_err(|e| format!("app {app_id}: {e}"))?;
            providers.insert(app_id.clone(), provider);
        }
        let dedup = DedupCache::new(
            std::time::Duration::from_secs(config.dedup_ttl_secs),
            config.dedup_max_entries,
        );
        Ok(AppState {
            config,
            providers,
            dedup,
        })
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/_matrix/push/v1/notify", post(notify))
        .route("/health", get(|| async { "ok" }))
        .with_state(state)
}

/// `POST /_matrix/push/v1/notify`
///
/// Answers 200 with the list of rejected pushkeys; the homeserver deletes
/// the pushers behind those keys, so only permanently invalid keys belong in
/// it. A transiently failed delivery instead fails the whole request with
/// 502 -- the homeserver retries, and duplicate suppression shields the
/// devices that were already delivered in the first attempt.
async fn notify(
    State(state): State<Arc<AppState>>,
    Json(req): Json<NotifyRequest>,
) -> Result<Json<NotifyResponse>, (StatusCode, String)> {
    let n = &req.notification;
    let mut rejected = Vec::new();
    let mut transient = 0usize;

    for device in &n.devices {
        match state.providers.get(&device.app_id) {
            Some(provider) => {
                // Retry suppression, keyed per device: count-only
                // notifications (no event_id) are idempotent per the spec
                // and always pass. Suppressed is still a success, never a
                // rejection -- the pushkey is perfectly valid.
                if let Some(event_id) = &n.event_id
                    && state
                        .dedup
                        .is_duplicate(event_id, &device.app_id, &device.pushkey)
                {
                    tracing::debug!(
                        app_id = %device.app_id,
                        pushkey = %device.pushkey,
                        event_id = %event_id,
                        "duplicate notification suppressed"
                    );
                    continue;
                }
                match provider.deliver(n, device).await {
                    Outcome::Delivered => {
                        if let Some(event_id) = &n.event_id {
                            state
                                .dedup
                                .record(event_id, &device.app_id, &device.pushkey);
                        }
                    }
                    Outcome::Rejected => {
                        tracing::info!(
                            app_id = %device.app_id,
                            pushkey = %device.pushkey,
                            "pushkey invalid, rejecting"
                        );
                        rejected.push(device.pushkey.clone());
                    }
                    Outcome::Skipped => {}
                    Outcome::Transient(e) => {
                        tracing::warn!(
                            app_id = %device.app_id,
                            pushkey = %device.pushkey,
                            error = %e,
                            "transient delivery failure"
                        );
                        transient += 1;
                    }
                }
            }
            None if state.config.reject_unknown_apps => {
                tracing::info!(app_id = %device.app_id, "unknown app_id, rejecting pushkey");
                rejected.push(device.pushkey.clone());
            }
            None => {
                tracing::warn!(app_id = %device.app_id, "unknown app_id, skipping device");
            }
        }
    }

    if transient > 0 {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("{transient} deliveries failed transiently, please retry"),
        ));
    }
    Ok(Json(NotifyResponse { rejected }))
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
        router(Arc::new(AppState::new(config).unwrap()))
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

    // --- FCM end-to-end against a mock server ---

    /// Throwaway RSA key generated FOR THESE TESTS ONLY (openssl genpkey);
    /// it authenticates nothing anywhere.
    const TEST_RSA_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDP8K93Jlsxp+QR
RlmCPhJLlSwW2bO0D00yqDB1zD7sE8UnTmepSg+kLOT7wUoEoHfqYPOM2cdTVAas
LHu9J1KFfYOTK0QrsL+7LrpRV82Hs/FtbHvTGhuoKHWpf6UyylucbaRNynuOa1A8
ymHTSV31UaWrq37PWzX8qx5PNIRvNwS0qrOmazWQXYoHG4ejobl5rEiYzwndnESW
7P2Gb3jNVBkIgg8vXjipnEOe/dZLAFzJOz15tiRv3832yakWbuuPUL7BgU9/8zm2
o9JIGG0xb49nqMwKM3j24N1aH6kA2x8EmLv87NXGZHtZx9v4Ij5fgL2/KEG7H8uu
wzLG5SOlAgMBAAECggEAXNFFKWzEGRqliXZ6/tGBLh7EguCjP9TysxFzLCnCznMW
tnBfgifuamylu6CwTvdn/4VOQYl2WUIxBkqG40x5n9+CSz9tWwk21DFL9oI4WoIe
WqcpcHX/cWS5/LJfBZhhIyanyBeBZnWNZ804tGzT1WygBEx1Os6ufv3M9jLtiIxa
2qMtZIG1msLUrc4kTtfRe6cLSlfrS0gUcWGz2QUstQuuPglld/3A6iAwQftlGXC6
bYiMSWBADg1FRVldxbqMxUJ58gS3nRgcHmcnvmam3iVqZZa4cXnYL9C0xNbvvedT
M1xN5n3yJ487aVjzncHgsmWUt7QXcTUva7b7DDCvYwKBgQDvznuLG7mjW5qUyo9M
O9cw7PCAzIYpAqUICisFJikwlHt7lhjyyq7EOJBF9Ts2UyvMJMAulxHpGpZ3RxiM
w9D7Ae29uL8ZVWLFoothurpyRpbBsPYyvvUx4fCnWKoOr+oJXsOUgivLC30dwYZO
CQsH+LximIZi/pawJh+eyPh4ewKBgQDd+1S43zO3YqcgKe5wrqSrQKADhVH8rXur
Tu8xn64sTdzlaxkMEbJkRCleimLiA5WlVQi9Z5MbQxuM0ujOTlE9oRH1Wf96EDDf
Rl0bsUGZFzifjTdp74ihP0tmpNPKQYxIwuLeiD7Gus3bPx9/+EHrELXzZT9+UHi8
zdfswLDqXwKBgQCJx6Ln2/geyYTZNEB81mzfKWNNPTVf3qsfIWhyPuivhsAj06tl
49nh13XdG/b3UXX6hqr8mcOqoKIOygRq7B7n+MW1ma4CSjLDxo46imSRP8liY+Aw
a9LI5D22iJS8d4oJ9C5+5wNuV519OTGHKF70J49lPqkHu6qsblsAigtofQKBgGEU
CjAzhNV9cmNxkxJ6fg9a2t/PTVS4te3sPlUwZSaBAsreNHz/vEl3ObRbxvTa5nYA
oyraAg6ZIZJLpn6a55KRP15SdpT2QblTd2Kl+W8vJZc5VfOhStph6OLB0NGSKvyj
Jj51zSZyCZcJmwgHFSTtEPWZ4NOn87V2PCkQ+A33AoGBAKMs/sf8JupQVN04FLUs
oBiNCWaE4/LkcPQQwHrdYMB7g6mm/w4FlSAyKaeiS8vT1XNRN/fsioiR5/X38sZo
tgz+jZx9zW5+HM9ltTybFbZHG/EIsYQsXMvzKbpXvEgMB+DhqBz045zvPWl9FHBa
tLxJR74/6WyQh19rA/hKULwq
-----END PRIVATE KEY-----
";

    /// Captured (authorization header, request body) pairs.
    type Captured = Arc<std::sync::Mutex<Vec<(String, serde_json::Value)>>>;

    /// A fake token endpoint plus a messages:send endpoint whose response is
    /// selected by the message token: tok-unregistered -> 404 UNREGISTERED,
    /// tok-unavailable -> 503, anything else -> success.
    async fn spawn_mock_fcm() -> (String, Captured) {
        use axum::response::IntoResponse;

        let captured: Captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let cap = captured.clone();
        let app = Router::new()
            .route(
                "/token",
                axum::routing::post(|| async {
                    Json(serde_json::json!({
                        "access_token": "test-bearer",
                        "expires_in": 3600,
                        "token_type": "Bearer",
                    }))
                }),
            )
            .route(
                "/v1/projects/test-project/messages:send",
                axum::routing::post(
                    move |headers: axum::http::HeaderMap, Json(body): Json<serde_json::Value>| {
                        let cap = cap.clone();
                        async move {
                            let auth = headers
                                .get(header::AUTHORIZATION)
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("")
                                .to_owned();
                            let token = body["message"]["token"].as_str().unwrap_or("").to_owned();
                            cap.lock().unwrap().push((auth, body));
                            match token.as_str() {
                                "tok-unregistered" => (
                                    StatusCode::NOT_FOUND,
                                    Json(serde_json::json!({"error": {
                                        "code": 404,
                                        "status": "NOT_FOUND",
                                        "details": [{"errorCode": "UNREGISTERED"}],
                                    }})),
                                )
                                    .into_response(),
                                "tok-unavailable" => (
                                    StatusCode::SERVICE_UNAVAILABLE,
                                    Json(serde_json::json!({"error": {
                                        "code": 503,
                                        "status": "UNAVAILABLE",
                                    }})),
                                )
                                    .into_response(),
                                _ => Json(serde_json::json!({
                                    "name": "projects/test-project/messages/1",
                                }))
                                .into_response(),
                            }
                        }
                    },
                ),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), captured)
    }

    /// Builds a router with one FCM app wired to the mock server.
    async fn fcm_router(api_root: &str) -> Router {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let sa_path = std::env::temp_dir().join(format!(
            "pincerbell-test-sa-{}-{seq}.json",
            std::process::id()
        ));
        std::fs::write(
            &sa_path,
            serde_json::json!({
                "type": "service_account",
                "project_id": "test-project",
                "private_key": TEST_RSA_KEY,
                "client_email": "test@test-project.iam.example.test",
                "token_uri": format!("{api_root}/token"),
            })
            .to_string(),
        )
        .unwrap();
        let config: Config = ::toml::from_str(&format!(
            r#"
            [apps."org.example.app"]
            kind = "fcm"
            service_account_file = {sa_path:?}
            api_root = "{api_root}"
            "#,
            sa_path = sa_path.display().to_string(),
        ))
        .unwrap();
        let router = router(Arc::new(AppState::new(config).unwrap()));
        std::fs::remove_file(sa_path).ok(); // read at startup, no longer needed
        router
    }

    #[tokio::test]
    async fn fcm_delivers_and_rejects_unregistered() {
        let (api_root, captured) = spawn_mock_fcm().await;
        let r = fcm_router(&api_root).await;

        let body = serde_json::json!({
            "notification": {
                "event_id": "$event1:example.test",
                "room_id": "!room:example.test",
                "counts": { "unread": 2 },
                "devices": [
                    { "app_id": "org.example.app", "pushkey": "tok-ok" },
                    { "app_id": "org.example.app", "pushkey": "tok-unregistered" },
                ],
            }
        });
        let (status, json) = notify_call(r, body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["rejected"], serde_json::json!(["tok-unregistered"]));

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 2);
        let (auth, msg) = &captured[0];
        assert_eq!(auth, "Bearer test-bearer");
        assert_eq!(msg["message"]["token"], "tok-ok");
        assert_eq!(msg["message"]["data"]["event_id"], "$event1:example.test");
        assert_eq!(msg["message"]["data"]["unread"], "2");
        assert_eq!(msg["message"]["android"]["priority"], "HIGH");
    }

    #[tokio::test]
    async fn fcm_transient_failure_asks_for_retry() {
        let (api_root, _captured) = spawn_mock_fcm().await;
        let r = fcm_router(&api_root).await;

        let (status, _) = notify_call(r, notification("org.example.app", "tok-unavailable")).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }

    // --- APNs end-to-end against a mock server ---

    /// Captured (headers of interest, body) pairs plus the device token
    /// from the URL.
    type ApnsCaptured = Arc<std::sync::Mutex<Vec<(String, serde_json::Value, serde_json::Value)>>>;

    /// Mock APNs: response selected by the device token in the URL:
    /// tok-gone -> 410 Unregistered, tok-throttle -> 429, else 200.
    async fn spawn_mock_apns() -> (String, ApnsCaptured) {
        use axum::response::IntoResponse;

        let captured: ApnsCaptured = Arc::new(std::sync::Mutex::new(Vec::new()));
        let cap = captured.clone();
        let app = Router::new().route(
            "/3/device/{token}",
            axum::routing::post(
                move |axum::extract::Path(token): axum::extract::Path<String>,
                      headers: axum::http::HeaderMap,
                      Json(body): Json<serde_json::Value>| {
                    let cap = cap.clone();
                    async move {
                        let h = |name: &str| {
                            headers
                                .get(name)
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("")
                                .to_owned()
                        };
                        let meta = serde_json::json!({
                            "authorization": h("authorization"),
                            "apns-topic": h("apns-topic"),
                            "apns-push-type": h("apns-push-type"),
                            "apns-priority": h("apns-priority"),
                        });
                        cap.lock().unwrap().push((token.clone(), meta, body));
                        match token.as_str() {
                            "tok-gone" => (
                                StatusCode::GONE,
                                Json(serde_json::json!({"reason": "Unregistered"})),
                            )
                                .into_response(),
                            "tok-throttle" => (
                                StatusCode::TOO_MANY_REQUESTS,
                                Json(serde_json::json!({"reason": "TooManyRequests"})),
                            )
                                .into_response(),
                            _ => StatusCode::OK.into_response(),
                        }
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), captured)
    }

    /// Builds a router with one APNs app wired to the mock server.
    async fn apns_router(api_root: &str) -> Router {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let key_path = std::env::temp_dir().join(format!(
            "pincerbell-test-apns-gw-{}-{seq}.p8",
            std::process::id()
        ));
        std::fs::write(&key_path, crate::apns::tests::TEST_EC_KEY).unwrap();
        let config: Config = ::toml::from_str(&format!(
            r#"
            [apps."org.example.iosapp"]
            kind = "apns"
            key_file = {key_path:?}
            key_id = "TESTKEY123"
            team_id = "TESTTEAM12"
            topic = "org.example.iosapp"
            api_root = "{api_root}"

            [apps."org.example.iosapp.voip"]
            kind = "apns"
            key_file = {key_path:?}
            key_id = "TESTKEY123"
            team_id = "TESTTEAM12"
            topic = "org.example.iosapp.voip"
            api_root = "{api_root}"
            push_type = "voip"
            "#,
            key_path = key_path.display().to_string(),
        ))
        .unwrap();
        let router = router(Arc::new(AppState::new(config).unwrap()));
        std::fs::remove_file(key_path).ok(); // read at startup, no longer needed
        router
    }

    #[tokio::test]
    async fn apns_delivers_and_rejects_unregistered() {
        let (api_root, captured) = spawn_mock_apns().await;
        let r = apns_router(&api_root).await;

        let body = serde_json::json!({
            "notification": {
                "event_id": "$event1:example.test",
                "room_id": "!room:example.test",
                "counts": { "unread": 2 },
                "devices": [
                    { "app_id": "org.example.iosapp", "pushkey": "tok-ios-ok" },
                    { "app_id": "org.example.iosapp", "pushkey": "tok-gone" },
                ],
            }
        });
        let (status, json) = notify_call(r, body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["rejected"], serde_json::json!(["tok-gone"]));

        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 2);
        let (token, meta, payload) = &captured[0];
        assert_eq!(token, "tok-ios-ok");
        assert!(
            meta["authorization"]
                .as_str()
                .unwrap()
                .starts_with("bearer ")
        );
        assert_eq!(meta["apns-topic"], "org.example.iosapp");
        assert_eq!(meta["apns-push-type"], "alert");
        assert_eq!(meta["apns-priority"], "10");
        assert_eq!(payload["aps"]["mutable-content"], 1);
        assert_eq!(payload["aps"]["badge"], 2);
        assert_eq!(payload["event_id"], "$event1:example.test");
    }

    #[tokio::test]
    async fn apns_transient_failure_asks_for_retry() {
        let (api_root, _captured) = spawn_mock_apns().await;
        let r = apns_router(&api_root).await;

        let (status, _) = notify_call(r, notification("org.example.iosapp", "tok-throttle")).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn apns_voip_app_sends_voip_type_at_full_priority() {
        let (api_root, captured) = spawn_mock_apns().await;
        let r = apns_router(&api_root).await;

        let body = serde_json::json!({
            "notification": {
                "event_id": "$call1:example.test",
                "room_id": "!room:example.test",
                "type": "m.call.invite",
                "prio": "low", // voip must STILL go out at full priority
                "devices": [
                    { "app_id": "org.example.iosapp.voip", "pushkey": "tok-voip" },
                ],
            }
        });
        let (status, json) = notify_call(r, body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["rejected"], serde_json::json!([]));

        let captured = captured.lock().unwrap();
        let (token, meta, payload) = &captured[0];
        assert_eq!(token, "tok-voip");
        assert_eq!(meta["apns-topic"], "org.example.iosapp.voip");
        assert_eq!(meta["apns-push-type"], "voip");
        assert_eq!(meta["apns-priority"], "10");
        assert!(payload["aps"].as_object().unwrap().is_empty());
        assert_eq!(payload["type"], "m.call.invite");
    }
}
