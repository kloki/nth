//! PKCE (RFC 7636) and the login's state: what keeps a code the browser
//! hands back useless to anyone but the nth that asked for it.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};

use crate::Error;

/// The characters RFC 7636 allows in a verifier.
const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
/// The shortest verifier the RFC allows, as opencode and Codex send.
const VERIFIER_LEN: usize = 43;

pub(crate) struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn new() -> Result<Self, Error> {
        let mut verifier = String::with_capacity(VERIFIER_LEN);
        // Bytes past the last whole multiple of the alphabet are drawn again,
        // so every character is equally likely.
        let limit = (256 / UNRESERVED.len() * UNRESERVED.len()) as u8;
        while verifier.len() < VERIFIER_LEN {
            for b in random::<64>()? {
                if b < limit && verifier.len() < VERIFIER_LEN {
                    verifier.push(UNRESERVED[b as usize % UNRESERVED.len()] as char);
                }
            }
        }
        let challenge = challenge(&verifier);
        Ok(Self {
            verifier,
            challenge,
        })
    }
}

/// The S256 challenge: base64url of the verifier's SHA-256, unpadded.
pub(crate) fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub(crate) fn state() -> Result<String, Error> {
    Ok(URL_SAFE_NO_PAD.encode(random::<32>()?))
}

fn random<const N: usize>() -> Result<[u8; N], Error> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(Error::Random)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_matches_rfc_7636_appendix_b() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifier_is_43_unreserved_characters() {
        let pkce = Pkce::new().unwrap();
        assert_eq!(pkce.verifier.len(), VERIFIER_LEN);
        assert!(pkce.verifier.bytes().all(|b| UNRESERVED.contains(&b)));
        assert_eq!(pkce.challenge, challenge(&pkce.verifier));
        assert_ne!(pkce.verifier, Pkce::new().unwrap().verifier);
    }

    #[test]
    fn state_is_32_bytes_base64url() {
        let state = state().unwrap();
        assert_eq!(URL_SAFE_NO_PAD.decode(&state).unwrap().len(), 32);
    }
}
