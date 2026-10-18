//! Token format and password hashing.

use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng;
use sha2::{Digest, Sha256};

/// Alphabet for the random half of a token.
const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
/// Length of the random half (Sanctum uses 40).
pub const TOKEN_LENGTH: usize = 40;

/// Creates a new plaintext token for a row id.
pub fn new_token(id: i64) -> String {
    let mut random = rand::rng();
    let secret: String = (0..TOKEN_LENGTH)
        .map(|_| ALPHABET[random.random_range(0..ALPHABET.len())] as char)
        .collect();

    format!("{id}|{secret}")
}

/// The value stored in `personal_access_tokens.token`.
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.trim().as_bytes());
    hex(&hasher.finalize())
}

/// Splits `<id>|<secret>` into its parts.
pub fn split_token(token: &str) -> Option<(i64, &str)> {
    let (prefix, secret) = token.trim().split_once('|')?;
    let id = prefix.parse().ok()?;

    if secret.is_empty() {
        None
    } else {
        Some((id, secret))
    }
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);

    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0F) as usize] as char);
    }

    out
}

/// Hashes a password with Argon2id.
pub fn hash_password(password: &str) -> Result<String> {
    use argon2::Argon2;
    use password_hash::PasswordHasher;

    // A random salt is generated for us; the default Argon2id parameters are
    // the OWASP recommendation for interactive logins.
    let hash = Argon2::default()
        .hash_password(password.as_bytes())
        .map_err(|error| anyhow::anyhow!("hashing password: {error}"))?;

    Ok(hash.to_string())
}

/// Verifies a password against a stored hash.
///
/// Supports Argon2 (new) and bcrypt (hashes migrated from the old server);
/// callers re-hash bcrypt successes with [`hash_password`].
pub fn verify_password(password: &str, stored: &str) -> bool {
    use password_hash::PasswordVerifier;
    use password_hash::phc::PasswordHash;

    if stored.starts_with("$2y$") || stored.starts_with("$2a$") || stored.starts_with("$2b$") {
        return bcrypt::verify(password, stored).unwrap_or(false);
    }

    match PasswordHash::new(stored) {
        Ok(parsed) => argon2::Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// True when a stored hash should be upgraded (anything but Argon2id).
pub fn needs_rehash(stored: &str) -> bool {
    !stored.starts_with("$argon2id$")
}

/// URL-safe base64 without padding (ids that must not contain `|`).
pub fn opaque_id(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_have_the_sanctum_shape() {
        let token = new_token(42);
        let (prefix, secret) = token.split_once('|').unwrap();

        assert_eq!(prefix, "42");
        assert_eq!(secret.len(), TOKEN_LENGTH);
        assert_eq!(split_token(&token).unwrap().0, 42);
    }

    #[test]
    fn hashes_are_sha256_hex() {
        let token = "1|abcdefghijklmnopqrstuvwxyz0123456789ABCD";
        let hash = hash_token(token);

        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(hash_token("1|other"), hash);
        assert_eq!(hash_token(&format!("  {token}\n")), hash);
    }

    #[test]
    fn malformed_tokens_are_rejected() {
        assert!(split_token("no-pipe").is_none());
        assert!(split_token("x|abc").is_none());
        assert!(split_token("1|").is_none());
    }

    #[test]
    fn argon2_round_trip() {
        let hash = hash_password("supersecret123").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password("supersecret123", &hash));
        assert!(!verify_password("wrong", &hash));
        assert!(!needs_rehash(&hash));
    }

    #[test]
    fn bcrypt_hashes_from_the_old_server_still_verify_and_are_upgraded() {
        let legacy = bcrypt::hash("supersecret123", 4).unwrap();

        assert!(verify_password("supersecret123", &legacy));
        assert!(!verify_password("nope", &legacy));
        assert!(needs_rehash(&legacy));
    }

    #[test]
    fn garbage_hashes_never_verify() {
        assert!(!verify_password("x", "not-a-hash"));
    }
}
