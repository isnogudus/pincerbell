//! Web Push delivery: RFC 8030 (HTTP push), RFC 8291 (payload encryption,
//! aes128gcm content coding per RFC 8188) and RFC 8292 (VAPID).
//!
//! Pusher contract: the pushkey is the subscription's P-256 ECDH public key
//! (base64url, 65-octet uncompressed point -- `getKey("p256dh")` of a
//! browser PushSubscription); the pusher data carries `endpoint` (the push
//! service URL) and `auth` (the 16-octet auth secret, `getKey("auth")`).
//!
//! The endpoint URL is CLIENT-CONTROLLED: without a check, the gateway
//! would POST to any URL a client names, i.e. act as an SSRF proxy into
//! its own network. `allowed_endpoints` is therefore a mandatory allowlist
//! of push-service hosts ("push.example.com" exact, "*.example.com" for
//! subdomains); endpoints outside it are skipped, never contacted.
//!
//! The encrypted payload carries the notification METADATA as JSON -- like
//! the other backends, never the event content.

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hkdf::Hkdf;
use p256::ecdsa::signature::Signer;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::pkcs8::DecodePrivateKey;
use sha2::Sha256;

use crate::api::{Device, Notification, Prio};
use crate::provider::Outcome;

/// How long the push service should retain the message if the device is
/// offline (the TTL header).
const TTL_SECS: u64 = 86_400;
/// VAPID token lifetime; RFC 8292 caps it at 24 hours.
const VAPID_LIFETIME: Duration = Duration::from_secs(12 * 60 * 60);
/// Reuse a cached VAPID token until an hour before it expires.
const VAPID_REFRESH_MARGIN: Duration = Duration::from_secs(60 * 60);

pub struct WebPushSettings {
    pub vapid_private_key: std::path::PathBuf,
    pub vapid_contact_email: String,
    pub allowed_endpoints: Vec<String>,
}

pub struct WebPushApp {
    client: reqwest::Client,
    signing_key: p256::ecdsa::SigningKey,
    /// Uncompressed public key for the `k=` parameter, base64url.
    public_key_b64: String,
    contact: String,
    allowed_endpoints: Vec<String>,
    /// VAPID tokens are audience-scoped: one cached token per endpoint
    /// origin.
    tokens: Mutex<std::collections::HashMap<String, CachedToken>>,
}

struct CachedToken {
    header: String,
    expires: Instant,
}

impl WebPushApp {
    pub fn new(settings: WebPushSettings) -> Result<WebPushApp, String> {
        if settings.allowed_endpoints.is_empty() {
            return Err(
                "webpush: allowed_endpoints must not be empty -- the endpoint URL is \
                 client-controlled, an empty allowlist would let clients aim the gateway \
                 at arbitrary URLs (SSRF)"
                    .to_owned(),
            );
        }
        let pem = std::fs::read_to_string(&settings.vapid_private_key)
            .map_err(|e| format!("{}: {e}", settings.vapid_private_key.display()))?;
        // Accept both PKCS#8 ("BEGIN PRIVATE KEY") and SEC1 ("BEGIN EC
        // PRIVATE KEY") encodings of the P-256 key.
        let secret = p256::SecretKey::from_pkcs8_pem(&pem)
            .or_else(|_| p256::SecretKey::from_sec1_pem(&pem))
            .map_err(|e| format!("{}: {e}", settings.vapid_private_key.display()))?;
        let public_key_b64 = B64.encode(secret.public_key().to_encoded_point(false).as_bytes());
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(WebPushApp {
            client,
            signing_key: p256::ecdsa::SigningKey::from(&secret),
            public_key_b64,
            contact: format!("mailto:{}", settings.vapid_contact_email),
            allowed_endpoints: settings.allowed_endpoints,
            tokens: Mutex::new(std::collections::HashMap::new()),
        })
    }

    pub async fn deliver(&self, n: &Notification, device: &Device) -> Outcome {
        // Unpack the subscription; anything malformed is a permanent
        // problem with THIS pusher, but rejecting would delete it -- skip
        // and log instead, mirroring the other backends' caution.
        let sub = match subscription(device) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(
                    app_id = %device.app_id,
                    error = %e,
                    "webpush: unusable subscription, skipping (not rejecting)"
                );
                return Outcome::Skipped;
            }
        };
        if let Err(e) = endpoint_allowed(&sub.endpoint, &self.allowed_endpoints) {
            tracing::error!(
                app_id = %device.app_id,
                endpoint = %sub.endpoint,
                error = %e,
                "webpush: endpoint not allowed, skipping (never contacted)"
            );
            return Outcome::Skipped;
        }

        let plaintext = build_payload(n).to_string();
        let as_secret = p256::SecretKey::random(&mut rand_core::OsRng);
        let mut salt = [0u8; 16];
        if let Err(e) = rand_core::RngCore::try_fill_bytes(&mut rand_core::OsRng, &mut salt) {
            return Outcome::Transient(format!("webpush: rng: {e}"));
        }
        let body = match encrypt(
            plaintext.as_bytes(),
            &sub.p256dh,
            &sub.auth,
            &as_secret,
            &salt,
        ) {
            Ok(b) => b,
            Err(e) => return Outcome::Transient(format!("webpush: {e}")),
        };

        let auth_header = match self.vapid_header(&sub.endpoint) {
            Ok(h) => h,
            Err(e) => return Outcome::Transient(format!("webpush: {e}")),
        };
        let resp = self
            .client
            .post(sub.endpoint.clone())
            .header("authorization", auth_header)
            .header("content-encoding", "aes128gcm")
            .header("content-type", "application/octet-stream")
            .header("ttl", TTL_SECS.to_string())
            .header(
                "urgency",
                match n.priority() {
                    Prio::High => "high",
                    Prio::Low => "normal",
                },
            )
            .body(body)
            .send()
            .await;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => return Outcome::Transient(format!("webpush: {e}")),
        };
        let status = resp.status();
        if status.is_success() {
            return Outcome::Delivered;
        }
        // 404/410 are the push service saying the subscription is gone for
        // good -- the one case that justifies deleting the pusher.
        if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
            return Outcome::Rejected;
        }
        let body = resp.text().await.unwrap_or_default();
        match status.as_u16() {
            401 | 403 | 408 | 429 => Outcome::Transient(format!("webpush: HTTP {status}")),
            s if status.is_server_error() => Outcome::Transient(format!("webpush: HTTP {s}")),
            _ => {
                tracing::error!(
                    app_id = %device.app_id,
                    status = %status,
                    body = %body.chars().take(500).collect::<String>(),
                    "webpush: permanent delivery failure, skipping (not rejecting)"
                );
                Outcome::Skipped
            }
        }
    }

    /// The RFC 8292 Authorization header, `vapid t=<jwt>, k=<public key>`,
    /// cached per endpoint origin (the JWT's audience).
    fn vapid_header(&self, endpoint: &reqwest::Url) -> Result<String, String> {
        let audience = endpoint.origin().ascii_serialization();
        if let Some(t) = self.tokens.lock().unwrap().get(&audience)
            && t.expires > Instant::now() + VAPID_REFRESH_MARGIN
        {
            return Ok(t.header.clone());
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before 1970")
            .as_secs();
        let header = B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = B64.encode(
            serde_json::json!({
                "aud": audience,
                "exp": now + VAPID_LIFETIME.as_secs(),
                "sub": self.contact,
            })
            .to_string(),
        );
        let signing_input = format!("{header}.{claims}");
        let signature: p256::ecdsa::Signature = self.signing_key.sign(signing_input.as_bytes());
        let jwt = format!("{signing_input}.{}", B64.encode(signature.to_bytes()));

        let value = format!("vapid t={jwt}, k={}", self.public_key_b64);
        self.tokens.lock().unwrap().insert(
            audience,
            CachedToken {
                header: value.clone(),
                expires: Instant::now() + VAPID_LIFETIME,
            },
        );
        Ok(value)
    }
}

struct Subscription {
    endpoint: reqwest::Url,
    p256dh: p256::PublicKey,
    auth: Vec<u8>,
}

/// Extracts endpoint/p256dh/auth from the device: pushkey = p256dh public
/// key, pusher data carries endpoint and auth.
fn subscription(device: &Device) -> Result<Subscription, String> {
    let p256dh = B64
        .decode(&device.pushkey)
        .map_err(|e| format!("pushkey is not base64url: {e}"))
        .and_then(|b| p256::PublicKey::from_sec1_bytes(&b).map_err(|e| format!("pushkey: {e}")))?;
    let data = device.data.as_ref().ok_or("pusher data missing")?;
    let endpoint = data
        .rest
        .get("endpoint")
        .and_then(|v| v.as_str())
        .ok_or("pusher data has no endpoint")?;
    let endpoint = reqwest::Url::parse(endpoint).map_err(|e| format!("endpoint: {e}"))?;
    let auth = data
        .rest
        .get("auth")
        .and_then(|v| v.as_str())
        .ok_or("pusher data has no auth secret")?;
    let auth = B64
        .decode(auth)
        .map_err(|e| format!("auth is not base64url: {e}"))?;
    Ok(Subscription {
        endpoint,
        p256dh,
        auth,
    })
}

/// The SSRF gate: https only (plain http solely for loopback, i.e. tests),
/// and the host must match the allowlist -- exact entry, or "*.suffix"
/// covering subdomains.
fn endpoint_allowed(url: &reqwest::Url, allowlist: &[String]) -> Result<(), String> {
    let host = url.host_str().ok_or("endpoint has no host")?;
    let loopback = host == "localhost" || host == "127.0.0.1" || host == "[::1]" || host == "::1";
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        s => return Err(format!("scheme {s} not allowed")),
    }
    let allowed = allowlist.iter().any(|entry| {
        if let Some(suffix) = entry.strip_prefix("*.") {
            host.len() > suffix.len() + 1
                && host.ends_with(suffix)
                && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
        } else {
            host == entry
        }
    });
    if allowed {
        Ok(())
    } else {
        Err(format!("host {host} not in allowed_endpoints"))
    }
}

/// The notification metadata as JSON -- the service worker fetches the
/// event itself; content is never included.
fn build_payload(n: &Notification) -> serde_json::Value {
    let mut payload = serde_json::Map::new();
    let mut put = |key: &str, value: Option<serde_json::Value>| {
        if let Some(v) = value {
            payload.insert(key.to_owned(), v);
        }
    };
    put("event_id", n.event_id.clone().map(Into::into));
    put("room_id", n.room_id.clone().map(Into::into));
    put("type", n.event_type.clone().map(Into::into));
    put("sender", n.sender.clone().map(Into::into));
    put(
        "sender_display_name",
        n.sender_display_name.clone().map(Into::into),
    );
    put("room_name", n.room_name.clone().map(Into::into));
    put(
        "prio",
        Some(match n.priority() {
            Prio::High => "high".into(),
            Prio::Low => "low".into(),
        }),
    );
    let counts = n.counts.as_ref();
    put("unread", counts.and_then(|c| c.unread).map(Into::into));
    put(
        "missed_calls",
        counts.and_then(|c| c.missed_calls).map(Into::into),
    );
    serde_json::Value::Object(payload)
}

/// RFC 8291 encryption with the aes128gcm content coding (RFC 8188): ECDH
/// over P-256, two HKDF stages, AES-128-GCM, all parameters explicit so the
/// RFC's Appendix A test vector can drive it directly.
fn encrypt(
    plaintext: &[u8],
    ua_public: &p256::PublicKey,
    auth: &[u8],
    as_secret: &p256::SecretKey,
    salt: &[u8; 16],
) -> Result<Vec<u8>, String> {
    let as_public = as_secret.public_key();
    let ua_pub_bytes = ua_public.to_encoded_point(false);
    let as_pub_bytes = as_public.to_encoded_point(false);

    let shared = p256::ecdh::diffie_hellman(as_secret.to_nonzero_scalar(), ua_public.as_affine());

    // IKM = HKDF(salt=auth, ikm=ecdh_secret) expanded with
    // "WebPush: info" || 0x00 || ua_public || as_public
    let mut key_info = Vec::with_capacity(14 + 65 + 65);
    key_info.extend_from_slice(b"WebPush: info\0");
    key_info.extend_from_slice(ua_pub_bytes.as_bytes());
    key_info.extend_from_slice(as_pub_bytes.as_bytes());
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(auth), shared.raw_secret_bytes())
        .expand(&key_info, &mut ikm)
        .map_err(|e| format!("hkdf ikm: {e}"))?;

    // CEK and nonce per RFC 8188.
    let hk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut cek = [0u8; 16];
    hk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
        .map_err(|e| format!("hkdf cek: {e}"))?;
    let mut nonce = [0u8; 12];
    hk.expand(b"Content-Encoding: nonce\0", &mut nonce)
        .map_err(|e| format!("hkdf nonce: {e}"))?;

    // Single record: plaintext, 0x02 delimiter (last record), GCM tag.
    let mut record = Vec::with_capacity(plaintext.len() + 1);
    record.extend_from_slice(plaintext);
    record.push(0x02);
    let ciphertext = Aes128Gcm::new(cek.as_slice().into())
        .encrypt(&Nonce::from(nonce), record.as_slice())
        .map_err(|e| format!("aes-gcm: {e}"))?;

    // aes128gcm header: salt || rs (u32 BE) || idlen || keyid (= as_public).
    let rs: u32 = 4096;
    let mut out = Vec::with_capacity(16 + 4 + 1 + 65 + ciphertext.len());
    out.extend_from_slice(salt);
    out.extend_from_slice(&rs.to_be_bytes());
    out.push(65);
    out.extend_from_slice(as_pub_bytes.as_bytes());
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn b64(s: &str) -> Vec<u8> {
        B64.decode(s).unwrap()
    }

    /// RFC 8291 Appendix A: fixed keys and salt must reproduce the RFC's
    /// encrypted message bit for bit.
    #[test]
    fn rfc8291_appendix_a_test_vector() {
        let plaintext = b64("V2hlbiBJIGdyb3cgdXAsIEkgd2FudCB0byBiZSBhIHdhdGVybWVsb24");
        let ua_public = p256::PublicKey::from_sec1_bytes(&b64(
            "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
        ))
        .unwrap();
        let as_secret =
            p256::SecretKey::from_slice(&b64("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw"))
                .unwrap();
        let auth = b64("BTBZMqHH6r4Tts7J_aSIgg");
        let salt: [u8; 16] = b64("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();

        let message = encrypt(&plaintext, &ua_public, &auth, &as_secret, &salt).unwrap();

        let expected = concat!(
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27ml",
            "mlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPT",
            "pK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
        );
        assert_eq!(B64.encode(&message), expected);
    }

    #[test]
    fn allowlist_matches_exact_and_wildcard() {
        let allow = vec![
            "fcm.googleapis.com".to_owned(),
            "*.push.apple.com".to_owned(),
        ];
        let ok = |u: &str| endpoint_allowed(&reqwest::Url::parse(u).unwrap(), &allow).is_ok();

        assert!(ok("https://fcm.googleapis.com/fcm/send/abc"));
        assert!(ok("https://web.push.apple.com/xyz"));
        assert!(
            !ok("https://push.apple.com/xyz"),
            "wildcard needs a subdomain"
        );
        assert!(!ok("https://evil.example.com/"));
        assert!(
            !ok("https://xpush.apple.com/"),
            "suffix match must respect the label boundary"
        );
        assert!(!ok("https://fcm.googleapis.com.evil.example/"));
    }

    #[test]
    fn allowlist_requires_https_except_loopback() {
        let allow = vec!["127.0.0.1".to_owned(), "internal.example.com".to_owned()];
        let check = |u: &str| endpoint_allowed(&reqwest::Url::parse(u).unwrap(), &allow);
        assert!(
            check("http://127.0.0.1:9999/push").is_ok(),
            "loopback http is for tests"
        );
        assert!(check("http://internal.example.com/push").is_err());
        assert!(check("https://internal.example.com/push").is_ok());
    }

    #[test]
    fn payload_carries_metadata_never_content() {
        let n: crate::api::NotifyRequest = serde_json::from_value(serde_json::json!({
            "notification": {
                "event_id": "$e1:example.test",
                "room_id": "!r:example.test",
                "counts": { "unread": 3 },
                "content": { "body": "must not be forwarded" },
                "devices": [{ "app_id": "org.example.web", "pushkey": "x" }]
            }
        }))
        .unwrap();
        let p = build_payload(&n.notification);
        assert_eq!(p["event_id"], "$e1:example.test");
        assert_eq!(p["unread"], 3);
        assert_eq!(p["prio"], "high");
        assert!(p.get("content").is_none());
    }

    /// Test-side decryption, the exact inverse of `encrypt`; used by the
    /// gateway end-to-end test to prove a subscriber can read the message.
    pub(crate) fn decrypt(message: &[u8], ua_secret: &p256::SecretKey, auth: &[u8]) -> Vec<u8> {
        let salt = &message[..16];
        let keyid = &message[21..86];
        let ciphertext = &message[86..];
        let as_public = p256::PublicKey::from_sec1_bytes(keyid).unwrap();
        let shared =
            p256::ecdh::diffie_hellman(ua_secret.to_nonzero_scalar(), as_public.as_affine());

        let ua_pub = ua_secret.public_key().to_encoded_point(false);
        let mut key_info = Vec::new();
        key_info.extend_from_slice(b"WebPush: info\0");
        key_info.extend_from_slice(ua_pub.as_bytes());
        key_info.extend_from_slice(keyid);
        let mut ikm = [0u8; 32];
        Hkdf::<Sha256>::new(Some(auth), shared.raw_secret_bytes())
            .expand(&key_info, &mut ikm)
            .unwrap();
        let hk = Hkdf::<Sha256>::new(Some(salt), &ikm);
        let mut cek = [0u8; 16];
        hk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
            .unwrap();
        let mut nonce = [0u8; 12];
        hk.expand(b"Content-Encoding: nonce\0", &mut nonce).unwrap();

        let mut record = Aes128Gcm::new(cek.as_slice().into())
            .decrypt(&Nonce::from(nonce), ciphertext)
            .unwrap();
        assert_eq!(record.pop(), Some(0x02), "last-record padding delimiter");
        record
    }

    #[test]
    fn empty_allowlist_is_refused_at_startup() {
        let result = WebPushApp::new(WebPushSettings {
            vapid_private_key: "/nonexistent.pem".into(),
            vapid_contact_email: "admin@example.test".into(),
            allowed_endpoints: vec![],
        });
        let Err(err) = result else {
            panic!("empty allowlist must be refused")
        };
        assert!(err.contains("allowed_endpoints"), "{err}");
    }

    #[test]
    fn encrypt_roundtrips_with_decrypt() {
        let ua_secret = p256::SecretKey::random(&mut rand_core::OsRng);
        let as_secret = p256::SecretKey::random(&mut rand_core::OsRng);
        let auth = b"0123456789abcdef";
        let salt = [7u8; 16];
        let msg = encrypt(b"hello", &ua_secret.public_key(), auth, &as_secret, &salt).unwrap();
        assert_eq!(decrypt(&msg, &ua_secret, auth), b"hello");
    }
}
