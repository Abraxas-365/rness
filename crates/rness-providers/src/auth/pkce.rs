//! PKCE (RFC 7636) helpers for the OAuth authorization-code flow.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// 32 random bytes, base64url without padding.
pub fn code_verifier() -> String {
    let mut b = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut b);
    URL_SAFE_NO_PAD.encode(b)
}

/// S256 challenge for a verifier.
pub fn code_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Random state parameter (CSRF protection).
pub fn state() -> String {
    let mut b = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut b);
    URL_SAFE_NO_PAD.encode(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_matches_rfc7636_appendix_b_vector() {
        // RFC 7636 §B test vector.
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifier_and_state_are_unique_and_urlsafe() {
        let v1 = code_verifier();
        let v2 = code_verifier();
        assert_ne!(v1, v2);
        assert_eq!(v1.len(), 43); // 32 bytes → 43 base64url chars
        assert!(!v1.contains('+') && !v1.contains('/') && !v1.contains('='));
        assert_ne!(state(), state());
    }
}
