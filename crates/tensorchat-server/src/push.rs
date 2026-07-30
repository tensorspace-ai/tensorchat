//! Web Push delivery, over VAPID.
//!
//! # What actually goes over the wire
//!
//! Nothing but a signed header. A push here has **no payload**: we POST an empty
//! body to the subscription's endpoint with a VAPID `Authorization` header, the
//! push service wakes the browser, and the service worker fetches the details
//! from `/api/me/notifications` on our own origin with the session cookie.
//!
//! The alternative — RFC 8291 encrypted payloads — would put message text into a
//! request to Google or Mozilla, protected by a key we derived. Correct, widely
//! used, and still a copy of the conversation leaving the building. The
//! round-trip this design costs happens on a device that has just been woken by
//! a network event anyway, and it buys the property that a self-hosted server
//! stays the only place message bodies exist.
//!
//! It also removes the entire ECDH/HKDF/AES-GCM path. What is left is one JWT.
//!
//! # VAPID
//!
//! A JWT signed with ES256 (ECDSA P-256 + SHA-256) whose claims are the push
//! service's origin, an expiry, and a contact address. The public key travels
//! beside it, and the browser checks it against the `applicationServerKey` the
//! subscription was created with — which is why the keypair must be stable
//! across restarts, and why it lives in the database rather than being minted at
//! boot.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::sec1::ToSec1Point;
use tensorchat_core::Id;
use tensorchat_store::Store;

use crate::state::Shared;

/// Settings key holding the VAPID private key, base64url-encoded raw scalar.
const VAPID_KEY: &str = "vapid_private_key";

/// How long a VAPID token is good for. Push services reject anything beyond 24
/// hours; an hour is plenty and keeps a leaked token short-lived.
const TOKEN_TTL_SECS: u64 = 60 * 60;

/// `TTL` header: how long the push service should hold a message for a device
/// that is offline. A chat notification is stale within the hour.
const PUSH_TTL_SECS: u32 = 60 * 60;

/// The VAPID identity of this server.
#[derive(Clone)]
pub struct Vapid {
    signing: SigningKey,
    /// Uncompressed SEC1 public point, base64url. Handed to the browser as the
    /// `applicationServerKey` and echoed in every `Authorization` header.
    pub public_b64: String,
    /// `sub` claim. Push services want a contact for the sender; `mailto:` or a
    /// URL are the accepted forms.
    subject: String,
}

impl Vapid {
    /// Load the keypair from the database, generating it on first use.
    ///
    /// Generation happens inside the store's `IMMEDIATE` transaction, so two
    /// processes starting together cannot each mint one and have the second
    /// overwrite the first — which would invalidate every subscription made
    /// against the first.
    pub fn load(store: &Store, subject: &str) -> Result<Vapid, String> {
        let encoded = store
            .setting_or_init(VAPID_KEY, generate_key)
            .map_err(|e| format!("reading the VAPID key: {e}"))?;

        let raw = URL_SAFE_NO_PAD
            .decode(&encoded)
            .map_err(|e| format!("the stored VAPID key is not base64url: {e}"))?;
        let signing = SigningKey::from_slice(&raw)
            .map_err(|e| format!("the stored VAPID key is not a P-256 scalar: {e}"))?;
        // Uncompressed (`false`): the browser's `applicationServerKey` is
        // specified as the 65-byte 0x04-prefixed form, and rejects a
        // compressed point.
        let public_b64 = URL_SAFE_NO_PAD.encode(
            signing
                .verifying_key()
                .as_affine()
                .to_sec1_point(false)
                .as_bytes(),
        );

        Ok(Vapid {
            signing,
            public_b64,
            subject: subject.to_string(),
        })
    }

    /// Build the `Authorization` header value for one push service origin.
    ///
    /// The `aud` claim is the *origin* of the endpoint, not the whole URL —
    /// push services reject a token scoped to the full path.
    fn authorization(&self, endpoint: &str, now_secs: u64) -> Result<String, String> {
        let audience = origin_of(endpoint).ok_or("push endpoint is not a valid URL")?;
        let header = br#"{"typ":"JWT","alg":"ES256"}"#;
        let claims = format!(
            r#"{{"aud":"{audience}","exp":{},"sub":"{}"}}"#,
            now_secs + TOKEN_TTL_SECS,
            self.subject
        );

        let mut jwt = String::with_capacity(220);
        jwt.push_str(&URL_SAFE_NO_PAD.encode(header));
        jwt.push('.');
        jwt.push_str(&URL_SAFE_NO_PAD.encode(claims.as_bytes()));

        // ES256 is the raw r‖s pair, not the DER encoding a general-purpose
        // ECDSA API hands back by default.
        let signature: Signature = self.signing.sign(jwt.as_bytes());
        jwt.push('.');
        jwt.push_str(&URL_SAFE_NO_PAD.encode(signature.to_bytes()));

        Ok(format!("vapid t={jwt}, k={}", self.public_b64))
    }
}

/// `scheme://host[:port]` of a URL, which is what a VAPID `aud` claim wants.
///
/// Written out rather than pulling in a URL parser: the input is a push
/// endpoint, always absolute, and this is the only place the server parses one.
fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty() || rest.is_empty() {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{authority}"))
}

/// The outcome of one delivery attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum Delivery {
    Sent,
    /// The push service has forgotten this subscription (404/410). The row
    /// should be deleted rather than retried — the browser will make a new one.
    Gone,
    /// Anything else. Counted against the endpoint; enough of them retire it.
    Failed,
}

/// Deliver one payload-less push.
pub async fn deliver(
    client: &reqwest::Client,
    vapid: &Vapid,
    endpoint: &str,
    now_secs: u64,
) -> Delivery {
    let Ok(authorization) = vapid.authorization(endpoint, now_secs) else {
        return Delivery::Failed;
    };

    let response = client
        .post(endpoint)
        .header("Authorization", authorization)
        .header("TTL", PUSH_TTL_SECS.to_string())
        // Required even with no body: a push service treats a missing
        // Content-Length as a malformed request rather than an empty one.
        .header("Content-Length", "0")
        .send()
        .await;

    match response {
        Ok(r) if r.status().is_success() => Delivery::Sent,
        Ok(r) if r.status() == 404 || r.status() == 410 => Delivery::Gone,
        Ok(r) => {
            tracing::debug!(status = %r.status(), "push rejected");
            Delivery::Failed
        }
        Err(e) => {
            tracing::debug!(error = %e, "push failed");
            Delivery::Failed
        }
    }
}

/// Wake every device belonging to `user`, unless they are already looking.
///
/// Spawned rather than awaited by the caller: a message send must not wait on a
/// round trip to Google, and a push service being slow is not a reason for a
/// message to be slow.
pub fn notify(st: &Shared, user: Id) {
    // Connected at all — even with the tab hidden — means the in-page notifier
    // already has this, and a push on top would be a duplicate buzz. The gap
    // this closes is the one where nothing is open.
    if st.hub.presence_of(user) != tensorchat_core::Presence::Offline {
        return;
    }
    let Some(vapid) = st.vapid.clone() else {
        return;
    };
    let st = st.clone();
    tokio::spawn(async move {
        let endpoints = match st.db(move |s| s.push_subscriptions_for(user)).await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "could not read push subscriptions");
                return;
            }
        };
        let now = tensorchat_core::now_ms() / 1000;
        for endpoint in endpoints {
            let outcome = deliver(&st.http, &vapid, &endpoint, now).await;
            let ep = endpoint.clone();
            let _ = st
                .db(move |s| match outcome {
                    Delivery::Sent => s.record_push_success(&ep),
                    Delivery::Gone => s.remove_push_subscription(&ep).map(|_| ()),
                    Delivery::Failed => s.record_push_failure(&ep),
                })
                .await;
        }
    });
}

/// Mint a P-256 private scalar, base64url-encoded.
///
/// Sampled directly from this crate's own RNG rather than through
/// `SigningKey::random`, for the same reason `auth.rs` generates its Argon2 salt
/// by hand: `p256` wants an RNG from a `rand_core` generation that `rand` 0.10
/// is a major version away from, and bridging the two is more code and more
/// risk than drawing 32 bytes.
///
/// A uniform 32-byte string is a valid scalar unless it is zero or at least the
/// curve order — a window of roughly 2^-128, which `from_slice` rejects. Looping
/// on that is correct and, in practice, never iterates.
fn generate_key() -> String {
    use rand::Rng as _;
    loop {
        let mut raw = [0u8; 32];
        rand::rng().fill_bytes(&mut raw);
        if let Ok(key) = SigningKey::from_slice(&raw) {
            return URL_SAFE_NO_PAD.encode(key.to_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vapid() -> Vapid {
        let store = Store::open_in_memory().unwrap();
        Vapid::load(&store, "mailto:ops@example.com").unwrap()
    }

    #[test]
    fn the_keypair_is_stable_across_loads() {
        // A new keypair would invalidate every subscription a browser has
        // already created against the old public key.
        let store = Store::open_in_memory().unwrap();
        let a = Vapid::load(&store, "mailto:ops@example.com").unwrap();
        let b = Vapid::load(&store, "mailto:ops@example.com").unwrap();
        assert_eq!(a.public_b64, b.public_b64);
    }

    #[test]
    fn the_public_key_is_an_uncompressed_p256_point() {
        // 0x04 followed by two 32-byte coordinates: the form the browser's
        // `applicationServerKey` expects.
        let v = vapid();
        let raw = URL_SAFE_NO_PAD.decode(&v.public_b64).unwrap();
        assert_eq!(raw.len(), 65);
        assert_eq!(raw[0], 0x04);
        assert!(!v.public_b64.contains(['+', '/', '=']), "must be url-safe");
    }

    #[test]
    fn the_authorization_header_is_a_verifiable_es256_jwt() {
        use p256::ecdsa::VerifyingKey;
        use p256::ecdsa::signature::Verifier;

        let v = vapid();
        let header = v
            .authorization("https://fcm.googleapis.com/fcm/send/abc123", 1_000)
            .unwrap();
        let jwt = header
            .strip_prefix("vapid t=")
            .unwrap()
            .split(',')
            .next()
            .unwrap();

        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "header.claims.signature");

        let head = String::from_utf8(URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        assert!(head.contains(r#""alg":"ES256""#), "got {head}");

        let claims = String::from_utf8(URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        // The audience is the origin, not the full endpoint — push services
        // reject a token scoped to the path.
        assert!(
            claims.contains(r#""aud":"https://fcm.googleapis.com""#),
            "got {claims}"
        );
        assert!(claims.contains(r#""sub":"mailto:ops@example.com""#));
        assert!(claims.contains(&format!(r#""exp":{}"#, 1_000 + TOKEN_TTL_SECS)));

        // The signature is over `header.claims` and verifies against the key
        // that travels in the same header.
        let signed = format!("{}.{}", parts[0], parts[1]);
        let sig = Signature::from_slice(&URL_SAFE_NO_PAD.decode(parts[2]).unwrap()).unwrap();
        let key_b64 = header.split("k=").nth(1).unwrap();
        let key = VerifyingKey::from_sec1_bytes(&URL_SAFE_NO_PAD.decode(key_b64).unwrap()).unwrap();
        key.verify(signed.as_bytes(), &sig)
            .expect("a push service must be able to verify this");
    }

    #[test]
    fn a_token_is_scoped_to_one_push_service() {
        // Otherwise a token captured by one service would authorize sends to
        // another, which is the whole point of the `aud` claim.
        let v = vapid();
        let a = v.authorization("https://fcm.googleapis.com/x", 1).unwrap();
        let b = v
            .authorization("https://updates.push.services.mozilla.com/y", 1)
            .unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn origins_are_extracted_without_a_url_parser() {
        assert_eq!(
            origin_of("https://fcm.googleapis.com/fcm/send/abc").as_deref(),
            Some("https://fcm.googleapis.com")
        );
        // A port is part of the origin.
        assert_eq!(
            origin_of("http://localhost:9012/push?x=1").as_deref(),
            Some("http://localhost:9012")
        );
        // No path at all.
        assert_eq!(
            origin_of("https://example.com").as_deref(),
            Some("https://example.com")
        );
        // A query or fragment immediately after the authority.
        assert_eq!(
            origin_of("https://example.com?a=b").as_deref(),
            Some("https://example.com")
        );
        for bad in ["", "not-a-url", "https://", "://example.com", "/relative"] {
            assert_eq!(origin_of(bad), None, "{bad:?} should not parse");
        }
    }

    #[test]
    fn a_malformed_endpoint_is_refused_rather_than_signed() {
        let v = vapid();
        assert!(v.authorization("not-a-url", 1).is_err());
    }
}
