//! Poll side of the queue/poll relay.
//!
//! One task per configured `[[poll]]` upstream: long-poll the queue-side
//! instance over HTTPS, run every fetched entry through the same delivery
//! pipeline the local notify endpoint uses (providers, duplicate
//! suppression), then acknowledge. Entries whose delivery failed
//! transiently are NOT acknowledged -- the upstream lease expiry is the
//! retry mechanism, taking the role the homeserver's 502-retry plays in
//! direct mode. Pushkeys a push service declared invalid are reported with
//! the ack; the queue side answers `rejected` to the homeserver's next
//! notify for them.

use std::sync::Arc;
use std::time::Duration;

use crate::api::Notification;
use crate::config::PollUpstream;
use crate::gateway::{AppState, Disposition, deliver_one};
use crate::queue::{AckRequest, PollRequest, PollResponse, RejectedPushkey};

/// A `[[poll]]` upstream with its token loaded and its HTTP client built;
/// fails fast at startup on an unreadable token file or a malformed proxy
/// URL, like the other credential files.
pub struct Upstream {
    url: String,
    token: String,
    timeout_secs: u64,
    max_batch: usize,
    client: reqwest::Client,
}

impl Upstream {
    pub fn load(cfg: &PollUpstream, default_proxy: Option<&str>) -> Result<Upstream, String> {
        // Per-upstream proxy wins over the top-level one; an empty string
        // opts out of both (http_client treats it as "direct").
        let proxy = cfg.proxy.as_deref().or(default_proxy);
        // No overall timeout: requests hold up to the long-poll duration;
        // poll_once bounds each request individually.
        let client = crate::provider::http_client(proxy, None)
            .map_err(|e| format!("poll {}: {e}", cfg.url))?;
        let raw = std::fs::read_to_string(&cfg.auth_token_file).map_err(|e| {
            format!(
                "poll auth_token_file {}: {e}",
                cfg.auth_token_file.display()
            )
        })?;
        let token = raw.trim();
        if token.is_empty() {
            return Err(format!(
                "poll auth_token_file {} is empty",
                cfg.auth_token_file.display()
            ));
        }
        Ok(Upstream {
            url: cfg.url.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
            timeout_secs: cfg.timeout_secs,
            max_batch: cfg.max_batch,
            client,
        })
    }
}

/// The per-upstream loop: poll, deliver, ack, forever. Connection errors
/// back off exponentially (1s..60s) and reset on the next success.
pub async fn run(state: Arc<AppState>, up: Upstream) {
    let mut backoff = Duration::from_secs(1);
    tracing::info!(url = %up.url, "polling upstream queue");
    loop {
        match poll_once(&state, &up).await {
            Ok(_) => backoff = Duration::from_secs(1),
            Err(e) => {
                tracing::warn!(url = %up.url, error = %e, "poll failed, backing off");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

/// One poll/deliver/ack cycle; returns the number of fetched entries. An
/// empty long-poll timeout is a normal Ok(0).
pub async fn poll_once(state: &AppState, up: &Upstream) -> Result<usize, String> {
    let resp = up
        .client
        .post(format!("{}/_pincerbell/v1/poll", up.url))
        .bearer_auth(&up.token)
        // Room on top of the server-side hold, not a second poll interval.
        .timeout(Duration::from_secs(up.timeout_secs + 30))
        .json(&PollRequest {
            timeout_secs: Some(up.timeout_secs),
            max: Some(up.max_batch),
        })
        .send()
        .await
        .map_err(|e| format!("poll: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("poll: upstream answered {}", resp.status()));
    }
    let resp: PollResponse = resp.json().await.map_err(|e| format!("poll body: {e}"))?;

    let fetched = resp.entries.len();
    let mut ack = AckRequest::default();
    for entry in resp.entries {
        let n: Notification = match serde_json::from_value(entry.notification) {
            Ok(n) => n,
            Err(e) => {
                // A malformed entry must not redeliver forever: ack it away.
                tracing::warn!(id = entry.id, error = %e, "unreadable queue entry, discarding");
                ack.acked.push(entry.id);
                continue;
            }
        };
        let mut transient = false;
        for device in &n.devices {
            match deliver_one(state, &n, device).await {
                Disposition::Fine => {}
                Disposition::Reject(pushkey) => ack.rejected_pushkeys.push(RejectedPushkey {
                    app_id: device.app_id.clone(),
                    pushkey,
                }),
                Disposition::Transient => transient = true,
            }
        }
        // Transient failure: leave unacked, the upstream lease redelivers.
        // (Our duplicate suppression shields any device of the entry that
        // did succeed.)
        if !transient {
            ack.acked.push(entry.id);
        }
    }

    if !ack.acked.is_empty() || !ack.rejected_pushkeys.is_empty() {
        let resp = up
            .client
            .post(format!("{}/_pincerbell/v1/ack", up.url))
            .bearer_auth(&up.token)
            .timeout(Duration::from_secs(30))
            .json(&ack)
            .send()
            .await
            .map_err(|e| format!("ack: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("ack: upstream answered {}", resp.status()));
        }
    }
    Ok(fetched)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::Config;
    use crate::gateway;

    /// Spawns a real queue-side pincerbell (token "relay-token") and returns
    /// its base URL.
    async fn spawn_queue_side() -> String {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let token_path = std::env::temp_dir().join(format!(
            "pincerbell-test-relay-token-{}-{seq}",
            std::process::id()
        ));
        std::fs::write(&token_path, "relay-token\n").unwrap();
        let config: Config = ::toml::from_str(&format!(
            r#"
            [queue]
            auth_token_file = {token_path:?}
            "#,
            token_path = token_path.display().to_string(),
        ))
        .unwrap();
        let router = gateway::router(Arc::new(gateway::AppState::new(config).unwrap()));
        std::fs::remove_file(token_path).ok();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    fn poll_side_state(toml: &str) -> AppState {
        let config: Config = ::toml::from_str(toml).unwrap();
        gateway::AppState::new(config).unwrap()
    }

    fn upstream(url: &str) -> Upstream {
        Upstream {
            url: url.to_owned(),
            token: "relay-token".to_owned(),
            timeout_secs: 0, // empty polls return immediately in tests
            max_batch: 100,
            client: reqwest::Client::new(),
        }
    }

    async fn notify(url: &str, app_id: &str, pushkey: &str) -> (u16, serde_json::Value) {
        let resp = reqwest::Client::new()
            .post(format!("{url}/_matrix/push/v1/notify"))
            .json(&serde_json::json!({
                "notification": {
                    "event_id": "$event1:example.test",
                    "room_id": "!room:example.test",
                    "counts": { "unread": 1 },
                    "devices": [{ "app_id": app_id, "pushkey": pushkey }]
                }
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap())
    }

    #[tokio::test]
    async fn end_to_end_queue_to_delivery() {
        let url = spawn_queue_side().await;
        let (status, json) = notify(&url, "org.example.app", "pk-1").await;
        assert_eq!(status, 200);
        assert_eq!(json["rejected"], serde_json::json!([]));

        let state = poll_side_state("[apps.\"org.example.app\"]\nkind = \"log\"");
        let n = poll_once(&state, &upstream(&url)).await.unwrap();
        assert_eq!(n, 1, "the queued entry is fetched and log-delivered");

        // Acked upstream: nothing left to fetch.
        let n = poll_once(&state, &upstream(&url)).await.unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn rejection_feeds_back_to_the_next_notify() {
        let url = spawn_queue_side().await;
        let (status, json) = notify(&url, "org.example.unknown", "pk-dead").await;
        assert_eq!(status, 200);
        assert_eq!(
            json["rejected"],
            serde_json::json!([]),
            "queued, not judged"
        );

        // Poll side knows no such app and is configured to reject.
        let state = poll_side_state("reject_unknown_apps = true");
        let n = poll_once(&state, &upstream(&url)).await.unwrap();
        assert_eq!(n, 1);

        // The NEXT notify for that pushkey now answers rejected.
        let (status, json) = notify(&url, "org.example.unknown", "pk-dead").await;
        assert_eq!(status, 200);
        assert_eq!(json["rejected"], serde_json::json!(["pk-dead"]));
    }

    #[tokio::test]
    async fn wrong_token_is_refused() {
        let url = spawn_queue_side().await;
        let state = poll_side_state("");
        let mut up = upstream(&url);
        up.token = "wrong".to_owned();
        let err = poll_once(&state, &up).await.unwrap_err();
        assert!(err.contains("401"), "got: {err}");
    }
}
