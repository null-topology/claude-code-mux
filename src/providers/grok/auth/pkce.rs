use base64::Engine;
use rand::Rng;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct PkceCodes {
    pub verifier: String,
    pub challenge: String,
}

fn encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn challenge_for(verifier: &str) -> String {
    encode(&Sha256::digest(verifier.as_bytes()))
}

pub fn generate_pkce() -> PkceCodes {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let verifier = encode(&bytes);
    let challenge = challenge_for(&verifier);
    PkceCodes {
        verifier,
        challenge,
    }
}

pub fn generate_state() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    encode(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_uses_s256() {
        let pkce = generate_pkce();
        assert_eq!(pkce.verifier.len(), 43);
        assert_eq!(
            pkce.challenge,
            encode(&Sha256::digest(pkce.verifier.as_bytes()))
        );
    }

    #[test]
    fn pkce_challenge_matches_rfc_7636_example() {
        // RFC 7636 Appendix B: the S256 challenge of its example verifier.
        assert_eq!(
            challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn state_is_url_safe() {
        let state = generate_state();
        assert_eq!(state.len(), 43);
        assert!(!state.contains(['+', '/', '=']));
    }
}
