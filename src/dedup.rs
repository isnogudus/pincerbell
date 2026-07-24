//! Duplicate suppression for homeserver retries.
//!
//! The spec requires gateways to use `event_id` to suppress duplicate
//! notifications when a homeserver retries after an HTTP error. Retries
//! happen within a bounded exponential-backoff window, so the gateway does
//! not need to remember every event_id ever seen: a TTL-bounded, size-capped
//! in-memory record of recent deliveries is sufficient. The key is the
//! (event_id, app_id, pushkey) triple -- the same event legitimately reaches
//! many devices; only the repeat delivery to the *same* device is a
//! duplicate. Count-only notifications carry no event_id and are idempotent
//! per the spec, so they never pass through here.
//!
//! The cache is deliberately not persistent: after a gateway restart a retry
//! may be delivered twice, which is a harmless double push notification.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

type Key = (String, String, String); // (event_id, app_id, pushkey)

pub struct DedupCache {
    ttl: Duration,
    max_entries: usize,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Delivery time per key, authoritative.
    seen: HashMap<Key, Instant>,
    /// Insertion order for expiry/eviction. May contain stale entries for
    /// keys that were re-recorded later; those are detected by comparing the
    /// stored timestamp against `seen` and skipped.
    order: VecDeque<(Instant, Key)>,
}

impl DedupCache {
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        DedupCache {
            ttl,
            max_entries,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Records a delivery and reports whether it duplicates one already
    /// recorded within the TTL: false = first sighting, deliver; true =
    /// duplicate, suppress. The suppression window is fixed from the first
    /// delivery -- a duplicate does not extend it.
    pub fn is_duplicate(&self, event_id: &str, app_id: &str, pushkey: &str) -> bool {
        self.is_duplicate_at(Instant::now(), event_id, app_id, pushkey)
    }

    fn is_duplicate_at(&self, now: Instant, event_id: &str, app_id: &str, pushkey: &str) -> bool {
        if self.ttl.is_zero() {
            return false; // dedup disabled
        }
        let mut inner = self.inner.lock().unwrap();

        // Expire from the front; entries whose timestamp no longer matches
        // `seen` are stale leftovers of a re-recorded key.
        while let Some((t, _)) = inner.order.front() {
            if now.duration_since(*t) < self.ttl {
                break;
            }
            let (t, key) = inner.order.pop_front().expect("front just checked");
            if inner.seen.get(&key) == Some(&t) {
                inner.seen.remove(&key);
            }
        }

        let key = (event_id.to_owned(), app_id.to_owned(), pushkey.to_owned());
        if let Some(&t) = inner.seen.get(&key)
            && now.duration_since(t) < self.ttl
        {
            return true;
        }

        inner.seen.insert(key.clone(), now);
        inner.order.push_back((now, key));

        // Size cap: evict oldest live entries first.
        while inner.seen.len() > self.max_entries {
            let Some((t, key)) = inner.order.pop_front() else {
                break;
            };
            if inner.seen.get(&key) == Some(&t) {
                inner.seen.remove(&key);
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(300);

    fn dedup(max: usize) -> DedupCache {
        DedupCache::new(TTL, max)
    }

    #[test]
    fn first_sighting_delivers_repeat_suppresses() {
        let d = dedup(1000);
        let now = Instant::now();
        assert!(!d.is_duplicate_at(now, "$e1", "app", "pk"));
        assert!(d.is_duplicate_at(now + Duration::from_secs(1), "$e1", "app", "pk"));
    }

    #[test]
    fn same_event_different_device_is_no_duplicate() {
        let d = dedup(1000);
        let now = Instant::now();
        assert!(!d.is_duplicate_at(now, "$e1", "app", "pk-a"));
        assert!(!d.is_duplicate_at(now, "$e1", "app", "pk-b"));
        assert!(!d.is_duplicate_at(now, "$e1", "other.app", "pk-a"));
    }

    #[test]
    fn entry_expires_after_ttl() {
        let d = dedup(1000);
        let now = Instant::now();
        assert!(!d.is_duplicate_at(now, "$e1", "app", "pk"));
        assert!(!d.is_duplicate_at(now + TTL, "$e1", "app", "pk"));
        // ... and the re-recording starts a fresh window.
        assert!(d.is_duplicate_at(now + TTL + Duration::from_secs(1), "$e1", "app", "pk"));
    }

    #[test]
    fn size_cap_evicts_oldest() {
        let d = dedup(2);
        let now = Instant::now();
        assert!(!d.is_duplicate_at(now, "$e1", "app", "pk"));
        assert!(!d.is_duplicate_at(now + Duration::from_secs(1), "$e2", "app", "pk"));
        assert!(!d.is_duplicate_at(now + Duration::from_secs(2), "$e3", "app", "pk"));
        // $e1 was evicted by the cap, $e2/$e3 are still present.
        assert!(!d.is_duplicate_at(now + Duration::from_secs(3), "$e1", "app", "pk"));
        assert!(d.is_duplicate_at(now + Duration::from_secs(3), "$e3", "app", "pk"));
    }

    #[test]
    fn re_recorded_key_survives_stale_order_entry() {
        let d = dedup(1000);
        let now = Instant::now();
        assert!(!d.is_duplicate_at(now, "$e1", "app", "pk"));
        // Expires, is re-recorded (leaving a stale order entry) ...
        assert!(!d.is_duplicate_at(now + TTL, "$e1", "app", "pk"));
        // ... a later pruning pass must not drop the fresh recording when it
        // discards the stale first-order entry.
        assert!(!d.is_duplicate_at(now + TTL, "$e2", "app", "pk"));
        assert!(d.is_duplicate_at(now + TTL + Duration::from_secs(1), "$e1", "app", "pk"));
    }

    #[test]
    fn zero_ttl_disables_dedup() {
        let d = DedupCache::new(Duration::ZERO, 1000);
        let now = Instant::now();
        assert!(!d.is_duplicate_at(now, "$e1", "app", "pk"));
        assert!(!d.is_duplicate_at(now, "$e1", "app", "pk"));
    }
}
