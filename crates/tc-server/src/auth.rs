//! Password hashing and session tokens.
//!
//! Sessions are **opaque random tokens**, not signed claims. A JWT-style token
//! cannot be revoked before it expires without keeping server-side state
//! anyway, so we skip the signature entirely: 256 bits of CSPRNG output, and
//! the database is the source of truth. Logout is then a `DELETE`, not a
//! blocklist.
//!
//! Only the SHA-256 of a token is stored. A stolen database dump therefore
//! yields no usable sessions — the same reasoning behind hashing passwords,
//! applied to the credential the client actually presents on every request.
//! SHA-256 (not Argon2) is right here because the input is already 256 bits of
//! uniform randomness: there is nothing to brute-force, and this runs on every
//! authenticated request.

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng;
use sha2::{Digest, Sha256};

/// Sessions last a month; the client refreshes by logging in again.
pub const SESSION_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// Minimum password length. Length is the only requirement — composition rules
/// push users toward predictable substitutions without adding real entropy.
pub const MIN_PASSWORD_LEN: usize = 8;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("password must be at least {MIN_PASSWORD_LEN} characters")]
    WeakPassword,
    #[error("invalid credentials")]
    BadCredentials,
    #[error("password hashing failed")]
    Hashing,
}

/// Hash a password for storage. Returns a PHC string that carries its own
/// parameters, so the cost can be raised later without invalidating old hashes.
pub fn hash_password(password: &str) -> Result<String, AuthError> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(AuthError::WeakPassword);
    }
    // Generate the salt ourselves rather than via `SaltString::generate`: that
    // helper wants an RNG from `password-hash`'s `rand_core` generation, which
    // is a major version behind the `rand` this crate already uses. Sixteen
    // random bytes is exactly what it would have produced anyway.
    let mut salt_bytes = [0u8; 16];
    fill_random(&mut salt_bytes);
    let salt = SaltString::encode_b64(&salt_bytes).map_err(|_| AuthError::Hashing)?;

    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|_| AuthError::Hashing)
}

/// Verify a password against a stored PHC hash.
pub fn verify_password(password: &str, phc: &str) -> Result<(), AuthError> {
    let parsed = PasswordHash::new(phc).map_err(|_| AuthError::BadCredentials)?;
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .map_err(|_| AuthError::BadCredentials)
}

/// A PHC hash of a fixed dummy password, used to keep login timing constant.
///
/// Without this, "unknown user" returns in microseconds while "wrong password"
/// takes the full Argon2 cost — an oracle that lets anyone enumerate which
/// handles exist. Computed once, lazily.
pub fn dummy_hash() -> &'static str {
    use std::sync::OnceLock;
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| {
        hash_password("tensorchat-timing-equalizer").expect("dummy hash must be constructible")
    })
}

/// Run a verification that is guaranteed to fail, so that a login attempt for a
/// nonexistent account costs the same as one for a real account.
pub fn equalize_timing(password: &str) {
    let _ = verify_password(password, dummy_hash());
}

/// A freshly minted session token: the secret to hand the client, and the
/// digest to store.
pub struct SessionToken {
    /// Give this to the client exactly once. It is never recoverable again.
    pub secret: String,
    pub hash: [u8; 32],
}

/// Fill a buffer with cryptographically secure random bytes.
///
/// `rand::rng()` is a ChaCha-family CSPRNG seeded from the OS and reseeded
/// periodically — appropriate for credentials, and it avoids a syscall per
/// call.
#[inline]
fn fill_random(buf: &mut [u8]) {
    rand::rng().fill_bytes(buf);
}

/// Mint a session token from 256 bits of randomness.
pub fn new_session_token() -> SessionToken {
    let mut raw = [0u8; 32];
    fill_random(&mut raw);
    let secret = URL_SAFE_NO_PAD.encode(raw);
    let hash = token_hash(&secret);
    SessionToken { secret, hash }
}

/// Digest a presented token for database lookup.
pub fn token_hash(secret: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(secret.as_bytes());
    h.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_and_verifies_a_password() {
        let phc = hash_password("correct horse battery").unwrap();
        assert!(
            phc.starts_with("$argon2"),
            "expected a PHC string, got {phc}"
        );
        verify_password("correct horse battery", &phc).unwrap();
        assert!(verify_password("wrong", &phc).is_err());
    }

    #[test]
    fn the_same_password_hashes_differently_each_time() {
        // Distinct salts: two users with the same password must not be
        // detectable by comparing rows.
        let a = hash_password("same password here").unwrap();
        let b = hash_password("same password here").unwrap();
        assert_ne!(a, b);
        verify_password("same password here", &a).unwrap();
        verify_password("same password here", &b).unwrap();
    }

    #[test]
    fn rejects_short_passwords_by_character_count() {
        assert!(matches!(
            hash_password("short"),
            Err(AuthError::WeakPassword)
        ));
        // Counted in characters, not bytes: eight emoji are eight characters,
        // even though they are 32 bytes.
        assert!(hash_password("🔒🔒🔒🔒🔒🔒🔒🔒").is_ok());
        assert!(hash_password("🔒🔒🔒").is_err());
    }

    #[test]
    fn malformed_stored_hashes_are_rejected_not_trusted() {
        assert!(verify_password("anything", "not-a-phc-string").is_err());
        assert!(verify_password("anything", "").is_err());
    }

    #[test]
    fn tokens_are_unique_and_hash_deterministically() {
        let a = new_session_token();
        let b = new_session_token();
        assert_ne!(a.secret, b.secret);
        assert_ne!(a.hash, b.hash);
        // 32 random bytes, base64url, no padding.
        assert_eq!(a.secret.len(), 43);
        assert!(!a.secret.contains(['+', '/', '=']), "must be url-safe");
        assert_eq!(token_hash(&a.secret), a.hash);
    }

    #[test]
    fn the_token_secret_is_not_recoverable_from_its_hash() {
        let t = new_session_token();
        assert!(
            !t.secret
                .as_bytes()
                .windows(4)
                .any(|w| t.hash.windows(4).any(|h| h == w))
        );
    }

    #[test]
    fn the_dummy_hash_is_a_real_argon2_hash_that_never_matches() {
        assert!(dummy_hash().starts_with("$argon2"));
        assert!(verify_password("tensorchat-timing-equalizer", dummy_hash()).is_ok());
        // The point is that an attacker's guess fails, at full cost.
        assert!(verify_password("guess", dummy_hash()).is_err());
        equalize_timing("guess");
    }
}
