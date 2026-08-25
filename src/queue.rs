//! Queue side of the queue/poll relay.
//!
//! When the systems that can reach the homeserver and the ones that can
//! reach the push services are not the same, one pincerbell instance runs in
//! queue mode: it accepts `/notify` as usual but holds the notifications in
//! a bounded in-memory ring buffer, and a second instance on the delivering
//! network fetches them over HTTPS long-poll (`/_pincerbell/v1/poll`) and
//! acknowledges (`/_pincerbell/v1/ack`).
//!
//! Semantics are at-least-once: a polled entry is leased, and an entry whose
//! ack never arrives is handed out again after the lease expires. The poll
//! side's duplicate suppression absorbs the resulting redeliveries. The
//! buffer is deliberately not persistent -- pushes are wake-up signals
//! without content, and a client resyncs on next open anyway; losing them on
//! restart is harmless, and it keeps the queue side dependency-free.
//!
//! Rejected pushkeys travel the only channel the push-gateway spec has: the
//! `rejected` list of a notify RESPONSE. By the time the poll side learns a
//! key is dead, the originating request is long answered, so the poll side
//! reports the key through the ack endpoint and the queue side answers
//! `rejected` to the NEXT notify that mentions it.

use std::collections::{HashMap, VecDeque};
use std::pin::pin;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::api::{Device, Notification};
use crate::config::QueueConfig;
use crate::dedup::DedupCache;

// --- wire types of the poll/ack endpoints (used by both sides) ---

/// Body of `POST /_pincerbell/v1/poll`.
#[derive(Debug, Serialize, Deserialize)]
pub struct PollRequest {
    /// Long-poll hold time when the queue is empty; the server caps it.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Maximum entries returned at once.
    #[serde(default)]
    pub max: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PollResponse {
    pub entries: Vec<PollEntry>,
}

/// One queued notification, always with exactly one device.
#[derive(Debug, Serialize, Deserialize)]
pub struct PollEntry {
    pub id: u64,
    pub notification: Value,
}

/// Body of `POST /_pincerbell/v1/ack`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AckRequest {
    /// Entry ids whose delivery is settled (delivered, suppressed, skipped,
    /// or rejected); unlisted entries redeliver after the lease expires.
    #[serde(default)]
    pub acked: Vec<u64>,
    /// Pushkeys the push service declared permanently invalid.
    #[serde(default)]
    pub rejected_pushkeys: Vec<RejectedPushkey>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RejectedPushkey {
    pub app_id: String,
    pub pushkey: String,
}

// --- queue-side state ---

/// Everything the queue-side endpoints need: the buffer, the shared-token
/// hash, and the known-invalid pushkeys awaiting report to the homeserver.
pub struct QueueState {
    pub queue: NotificationQueue,
    /// SHA-256 of the shared bearer token; comparing digests instead of the
    /// tokens themselves keeps the comparison timing useless to an attacker.
    token_hash: [u8; 32],
    /// (app_id, pushkey) pairs reported invalid by the poll side, as a
    /// TTL/size-bounded record -- the DedupCache with a fixed event_id is
    /// exactly that shape.
    rejected: DedupCache,
}

impl QueueState {
    pub fn new(cfg: &QueueConfig) -> Result<Self, String> {
        let raw = std::fs::read_to_string(&cfg.auth_token_file).map_err(|e| {
            format!(
                "queue auth_token_file {}: {e}",
                cfg.auth_token_file.display()
            )
        })?;
        let token = raw.trim();
        if token.is_empty() {
            return Err(format!(
                "queue auth_token_file {} is empty",
                cfg.auth_token_file.display()
            ));
        }
        Ok(QueueState {
            queue: NotificationQueue::new(
                cfg.max_entries,
                Duration::from_secs(cfg.entry_ttl_secs),
                Duration::from_secs(cfg.lease_secs),
            ),
            token_hash: Sha256::digest(token.as_bytes()).into(),
            rejected: DedupCache::new(Duration::from_secs(cfg.rejected_ttl_secs), 65_536),
        })
    }

    pub fn verify_token(&self, presented: &str) -> bool {
        let presented: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        presented == self.token_hash
    }

    pub fn report_rejected(&self, app_id: &str, pushkey: &str) {
        self.rejected.record("", app_id, pushkey);
    }

    pub fn is_rejected(&self, app_id: &str, pushkey: &str) -> bool {
        self.rejected.is_duplicate("", app_id, pushkey)
    }
}

// --- the ring buffer ---

struct Entry {
    id: u64,
    enqueued: Instant,
    payload: Value,
    /// Set for count-only notifications: the (app_id, pushkey) coalescing
    /// key. Only the newest count-only entry per key (per `count_index`) is
    /// live; older ones are dropped when they surface.
    count_key: Option<(String, String)>,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    pending: VecDeque<Entry>,
    /// id of the newest count-only entry per (app_id, pushkey); an entry
    /// whose id no longer matches is stale and gets dropped, never handed
    /// out.
    count_index: HashMap<(String, String), u64>,
    /// Polled but not yet acknowledged entries, with their lease deadline.
    leased: HashMap<u64, (Instant, Entry)>,
}

pub struct NotificationQueue {
    max_entries: usize,
    entry_ttl: Duration,
    lease: Duration,
    notify: tokio::sync::Notify,
    inner: Mutex<Inner>,
}

impl NotificationQueue {
    pub fn new(max_entries: usize, entry_ttl: Duration, lease: Duration) -> Self {
        NotificationQueue {
            max_entries,
            entry_ttl,
            lease,
            notify: tokio::sync::Notify::new(),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Enqueues one (notification, device) pair as a single-device entry.
    /// Count-only notifications (no event_id) coalesce per device: only the
    /// newest badge state is delivered.
    pub fn enqueue(&self, n: &Notification, device: &Device) -> Result<(), String> {
        let mut payload = serde_json::to_value(n).map_err(|e| e.to_string())?;
        payload["devices"] = serde_json::json!([device]);
        let count_key = n
            .event_id
            .is_none()
            .then(|| (device.app_id.clone(), device.pushkey.clone()));
        self.enqueue_at(Instant::now(), payload, count_key);
        self.notify.notify_waiters();
        Ok(())
    }

    fn enqueue_at(&self, now: Instant, payload: Value, count_key: Option<(String, String)>) {
        let mut inner = self.inner.lock().unwrap();
        let id = inner.next_id;
        inner.next_id += 1;
        if let Some(k) = &count_key {
            inner.count_index.insert(k.clone(), id);
        }
        while inner.pending.len() >= self.max_entries {
            let Some(dropped) = inner.pending.pop_front() else {
                break;
            };
            if Self::is_live(&inner, &dropped) {
                Self::forget_count_key(&mut inner, &dropped);
                tracing::warn!(
                    id = dropped.id,
                    "queue full, dropping oldest entry undelivered"
                );
            }
        }
        inner.pending.push_back(Entry {
            id,
            enqueued: now,
            payload,
            count_key,
        });
    }

    /// A stale count-only entry has been superseded by a newer badge state.
    fn is_live(inner: &Inner, entry: &Entry) -> bool {
        match &entry.count_key {
            Some(k) => inner.count_index.get(k) == Some(&entry.id),
            None => true,
        }
    }

    /// Drops the count-index mapping if this entry still owns it.
    fn forget_count_key(inner: &mut Inner, entry: &Entry) {
        if let Some(k) = &entry.count_key
            && inner.count_index.get(k) == Some(&entry.id)
        {
            inner.count_index.remove(k);
        }
    }

    /// Returns up to `max` entries immediately, or waits until either one
    /// arrives or `timeout` passes (then an empty vec).
    pub async fn poll_wait(&self, max: usize, timeout: Duration) -> Vec<PollEntry> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // Register as a waiter BEFORE checking, so an enqueue between
            // the check and the await cannot be missed.
            let mut notified = pin!(self.notify.notified());
            notified.as_mut().enable();
            let batch = self.take_at(Instant::now(), max);
            if !batch.is_empty() {
                return batch;
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep_until(deadline) => return Vec::new(),
            }
        }
    }

    /// Non-waiting fetch: reclaims expired leases, drops expired/stale
    /// entries, leases and returns up to `max`.
    fn take_at(&self, now: Instant, max: usize) -> Vec<PollEntry> {
        let mut inner = self.inner.lock().unwrap();

        // Expired leases go back to the front (they are the oldest entries);
        // descending id order so the front ends up ascending.
        let mut expired: Vec<u64> = inner
            .leased
            .iter()
            .filter(|(_, (deadline, _))| *deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        expired.sort_unstable_by(|a, b| b.cmp(a));
        for id in expired {
            let (_, entry) = inner.leased.remove(&id).expect("id from iteration above");
            if !Self::is_live(&inner, &entry) {
                continue;
            }
            if now.duration_since(entry.enqueued) >= self.entry_ttl {
                Self::forget_count_key(&mut inner, &entry);
                continue;
            }
            tracing::debug!(id, "lease expired, entry requeued");
            inner.pending.push_front(entry);
        }

        let mut batch = Vec::new();
        while batch.len() < max {
            let Some(entry) = inner.pending.pop_front() else {
                break;
            };
            if !Self::is_live(&inner, &entry) {
                continue;
            }
            if now.duration_since(entry.enqueued) >= self.entry_ttl {
                Self::forget_count_key(&mut inner, &entry);
                tracing::debug!(id = entry.id, "entry expired undelivered");
                continue;
            }
            batch.push(PollEntry {
                id: entry.id,
                notification: entry.payload.clone(),
            });
            inner.leased.insert(entry.id, (now + self.lease, entry));
        }
        batch
    }

    /// Settles leased entries; unknown ids (double ack, ack after lease
    /// expiry) are ignored.
    pub fn ack(&self, ids: &[u64]) {
        let mut inner = self.inner.lock().unwrap();
        for id in ids {
            if let Some((_, entry)) = inner.leased.remove(id) {
                Self::forget_count_key(&mut inner, &entry);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(3600);
    const LEASE: Duration = Duration::from_secs(60);

    fn queue(max: usize) -> NotificationQueue {
        NotificationQueue::new(max, TTL, LEASE)
    }

    fn notification(event_id: Option<&str>) -> Notification {
        serde_json::from_value(serde_json::json!({
            "event_id": event_id,
            "room_id": "!room:example.test",
            "counts": { "unread": 1 },
            "devices": [
                { "app_id": "org.example.app", "pushkey": "pk-other" },
                { "app_id": "org.example.app", "pushkey": "pk-1" },
            ],
        }))
        .unwrap()
    }

    fn enqueue(q: &NotificationQueue, event_id: Option<&str>, pushkey: &str) {
        let n = notification(event_id);
        let device: Device = serde_json::from_value(serde_json::json!({
            "app_id": "org.example.app",
            "pushkey": pushkey,
        }))
        .unwrap();
        q.enqueue(&n, &device).unwrap();
    }

    #[test]
    fn enqueue_take_ack_cycle() {
        let q = queue(100);
        enqueue(&q, Some("$e1"), "pk-1");
        let now = Instant::now();

        let batch = q.take_at(now, 10);
        assert_eq!(batch.len(), 1);
        // Single-device payload, regardless of how many devices the
        // original notification had.
        let n = &batch[0].notification;
        assert_eq!(n["devices"].as_array().unwrap().len(), 1);
        assert_eq!(n["devices"][0]["pushkey"], "pk-1");
        assert_eq!(n["event_id"], "$e1");

        // Leased: a second take yields nothing.
        assert!(q.take_at(now, 10).is_empty());

        q.ack(&[batch[0].id]);
        // Settled: not redelivered even after the lease window.
        assert!(q.take_at(now + LEASE, 10).is_empty());
    }

    #[test]
    fn unacked_lease_redelivers_in_order() {
        let q = queue(100);
        enqueue(&q, Some("$e1"), "pk-1");
        enqueue(&q, Some("$e2"), "pk-1");
        let now = Instant::now();

        let batch = q.take_at(now, 10);
        assert_eq!(batch.len(), 2);
        // No ack: after the lease expires both come back, oldest first.
        let again = q.take_at(now + LEASE, 10);
        assert_eq!(again.len(), 2);
        assert_eq!(again[0].id, batch[0].id);
        assert_eq!(again[1].id, batch[1].id);
    }

    #[test]
    fn entry_ttl_drops_undelivered() {
        let q = queue(100);
        enqueue(&q, Some("$e1"), "pk-1");
        assert!(q.take_at(Instant::now() + TTL, 10).is_empty());
    }

    #[test]
    fn leased_entry_expired_by_ttl_is_not_requeued() {
        let q = NotificationQueue::new(100, LEASE, LEASE); // ttl == lease
        enqueue(&q, Some("$e1"), "pk-1");
        let now = Instant::now();
        assert_eq!(q.take_at(now, 10).len(), 1);
        assert!(q.take_at(now + LEASE, 10).is_empty());
    }

    #[test]
    fn cap_evicts_oldest() {
        let q = queue(2);
        enqueue(&q, Some("$e1"), "pk-1");
        enqueue(&q, Some("$e2"), "pk-1");
        enqueue(&q, Some("$e3"), "pk-1");
        let batch = q.take_at(Instant::now(), 10);
        let events: Vec<_> = batch
            .iter()
            .map(|e| e.notification["event_id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(events, ["$e2", "$e3"]);
    }

    #[test]
    fn count_only_coalesces_per_device() {
        let q = queue(100);
        enqueue(&q, None, "pk-1");
        enqueue(&q, None, "pk-1"); // supersedes the first
        enqueue(&q, None, "pk-2"); // different device, kept
        let batch = q.take_at(Instant::now(), 10);
        let pushkeys: Vec<_> = batch
            .iter()
            .map(|e| e.notification["devices"][0]["pushkey"].as_str().unwrap())
            .collect();
        assert_eq!(pushkeys, ["pk-1", "pk-2"]);
    }

    #[test]
    fn leased_count_only_superseded_while_out_is_not_requeued() {
        let q = queue(100);
        enqueue(&q, None, "pk-1");
        let now = Instant::now();
        assert_eq!(q.take_at(now, 10).len(), 1);
        // A newer badge state arrives while the old one is leased and
        // unacked: only the new one survives the lease expiry.
        enqueue(&q, None, "pk-1");
        let batch = q.take_at(now + LEASE, 10);
        assert_eq!(batch.len(), 1);
        q.ack(&[batch[0].id]);
        assert!(q.take_at(now + LEASE + LEASE, 10).is_empty());
    }

    #[test]
    fn event_notifications_do_not_coalesce() {
        let q = queue(100);
        enqueue(&q, Some("$e1"), "pk-1");
        enqueue(&q, Some("$e2"), "pk-1");
        assert_eq!(q.take_at(Instant::now(), 10).len(), 2);
    }

    #[tokio::test]
    async fn poll_wait_wakes_on_enqueue() {
        let q = std::sync::Arc::new(queue(100));
        let waiter = {
            let q = q.clone();
            tokio::spawn(async move { q.poll_wait(10, Duration::from_secs(10)).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        enqueue(&q, Some("$e1"), "pk-1");
        let batch = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("waiter must wake well before its 10s timeout")
            .unwrap();
        assert_eq!(batch.len(), 1);
    }

    #[tokio::test]
    async fn poll_wait_times_out_empty() {
        let q = queue(100);
        let batch = q.poll_wait(10, Duration::from_millis(20)).await;
        assert!(batch.is_empty());
    }

    #[test]
    fn queue_state_token_and_rejected_feedback() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("pincerbell-test-qtoken-{}", std::process::id()));
        std::fs::write(&path, "s3cret-token\n").unwrap();
        let cfg: QueueConfig = toml::from_str(&format!(
            "auth_token_file = {:?}",
            path.display().to_string()
        ))
        .unwrap();
        let qs = QueueState::new(&cfg).unwrap();
        std::fs::remove_file(&path).unwrap();

        assert!(qs.verify_token("s3cret-token"), "trimmed token matches");
        assert!(!qs.verify_token("wrong"));

        assert!(!qs.is_rejected("org.example.app", "pk-1"));
        qs.report_rejected("org.example.app", "pk-1");
        assert!(qs.is_rejected("org.example.app", "pk-1"));
        assert!(!qs.is_rejected("org.example.app", "pk-2"));
    }

    #[test]
    fn empty_token_file_is_an_error() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "pincerbell-test-qtoken-empty-{}",
            std::process::id()
        ));
        std::fs::write(&path, "  \n").unwrap();
        let cfg: QueueConfig = toml::from_str(&format!(
            "auth_token_file = {:?}",
            path.display().to_string()
        ))
        .unwrap();
        assert!(QueueState::new(&cfg).is_err());
        std::fs::remove_file(&path).unwrap();
    }
}
