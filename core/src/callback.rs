//! Callback signing helpers.
//!
//! Provides HMAC-SHA256 signing for webhook/callback payloads so
//! downstream consumers can verify authenticity.

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Compute a hex-encoded HMAC-SHA256 signature for a callback body.
///
/// # Panics
///
/// This function panics only if the HMAC implementation fails to
/// initialize from the given key, which cannot happen with the
/// `hmac` crate (it accepts keys of any length).
pub fn sign_callback_payload(secret: &str, body: &[u8]) -> String {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC can take key of any size");
    mac.update(body);
    let result = mac.finalize();
    hex::encode(result.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sign_callback_payload() {
        let secret = "my-secret";
        let body = b"hello world";
        let sig = sign_callback_payload(secret, body);
        assert!(!sig.is_empty());
        assert_eq!(sig.len(), 64); // hex-encoded SHA256 = 64 chars
    }

    #[test]
    fn test_sign_callback_payload_deterministic() {
        let secret = "my-secret";
        let body = b"hello world";
        let sig1 = sign_callback_payload(secret, body);
        let sig2 = sign_callback_payload(secret, body);
        assert_eq!(sig1, sig2);
    }

    #[test]
    fn test_sign_callback_payload_different_secrets() {
        let body = b"hello world";
        let sig1 = sign_callback_payload("secret-a", body);
        let sig2 = sign_callback_payload("secret-b", body);
        assert_ne!(sig1, sig2);
    }

    #[test]
    fn test_sign_callback_payload_different_bodies() {
        let secret = "my-secret";
        let sig1 = sign_callback_payload(secret, b"body-a");
        let sig2 = sign_callback_payload(secret, b"body-b");
        assert_ne!(sig1, sig2);
    }
}
