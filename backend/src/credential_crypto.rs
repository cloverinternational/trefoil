//! Authenticated encryption for OAuth credentials.
//!
//! Contract: AES-256-GCM, encoded as `v1:<base64(nonce || ciphertext+tag)>`.
//! The provider, account, and field name are authenticated as AAD, so a
//! ciphertext cannot be moved between credentials or token fields. Plaintext
//! rows are intentionally rejected; there is no implicit migration path.

use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes256Gcm, Nonce,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use rand::RngCore;
use thiserror::Error;

const VERSION: &str = "v1:";
const NONCE_LEN: usize = 12;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CredentialCryptoError {
    #[error("TREFOIL_CREDENTIAL_KEY is required")]
    MissingKey,
    #[error("TREFOIL_CREDENTIAL_KEY must be exactly 32 bytes of hex")]
    InvalidKey,
    #[error("invalid credential ciphertext envelope")]
    InvalidEnvelope,
    #[error("credential ciphertext authentication failed")]
    AuthenticationFailed,
}

#[derive(Clone)]
pub struct CredentialKey([u8; 32]);

impl CredentialKey {
    pub fn from_hex(value: &str) -> Result<Self, CredentialCryptoError> {
        let bytes = hex::decode(value).map_err(|_| CredentialCryptoError::InvalidKey)?;
        bytes
            .try_into()
            .map(Self)
            .map_err(|_| CredentialCryptoError::InvalidKey)
    }

    pub fn from_env() -> Result<Self, CredentialCryptoError> {
        match std::env::var("TREFOIL_CREDENTIAL_KEY") {
            Ok(value) => Self::from_hex(&value),
            Err(std::env::VarError::NotPresent) => Err(CredentialCryptoError::MissingKey),
            Err(std::env::VarError::NotUnicode(_)) => Err(CredentialCryptoError::InvalidKey),
        }
    }
}

fn aad(provider: &str, account: &str, field: &str) -> Vec<u8> {
    // Length prefixes avoid collisions when identifiers contain separators.
    let mut output = b"trefoil/oauth/v1".to_vec();
    for component in [provider, account, field] {
        output.extend_from_slice(&(component.len() as u64).to_be_bytes());
        output.extend_from_slice(component.as_bytes());
    }
    output
}

pub fn encrypt(
    key: &CredentialKey,
    provider: &str,
    account: &str,
    field: &str,
    plaintext: &str,
) -> Result<String, CredentialCryptoError> {
    let cipher = Aes256Gcm::new_from_slice(&key.0).expect("32-byte AES key");
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce_bytes),
            Payload {
                msg: plaintext.as_bytes(),
                aad: &aad(provider, account, field),
            },
        )
        .map_err(|_| CredentialCryptoError::AuthenticationFailed)?;
    let mut encoded = nonce_bytes.to_vec();
    encoded.extend(ciphertext);
    Ok(format!("{VERSION}{}", BASE64.encode(encoded)))
}

pub fn decrypt(
    key: &CredentialKey,
    provider: &str,
    account: &str,
    field: &str,
    encoded: &str,
) -> Result<String, CredentialCryptoError> {
    let payload = encoded
        .strip_prefix(VERSION)
        .ok_or(CredentialCryptoError::InvalidEnvelope)?;
    let bytes = BASE64
        .decode(payload)
        .map_err(|_| CredentialCryptoError::InvalidEnvelope)?;
    if bytes.len() < NONCE_LEN + 16 {
        return Err(CredentialCryptoError::InvalidEnvelope);
    }
    let cipher = Aes256Gcm::new_from_slice(&key.0).expect("32-byte AES key");
    let plaintext = cipher
        .decrypt(
            Nonce::from_slice(&bytes[..NONCE_LEN]),
            Payload {
                msg: &bytes[NONCE_LEN..],
                aad: &aad(provider, account, field),
            },
        )
        .map_err(|_| CredentialCryptoError::AuthenticationFailed)?;
    String::from_utf8(plaintext).map_err(|_| CredentialCryptoError::AuthenticationFailed)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key() -> CredentialKey {
        CredentialKey::from_hex(&"11".repeat(32)).unwrap()
    }
    #[test]
    fn round_trip_and_empty_refresh() {
        let k = key();
        for value in ["secret", "", "héllo"] {
            let c = encrypt(&k, "provider", "account", "refresh_token", value).unwrap();
            assert_eq!(
                decrypt(&k, "provider", "account", "refresh_token", &c).unwrap(),
                value
            );
        }
    }
    #[test]
    fn tamper_wrong_key_and_swap_rejected() {
        let k = key();
        let other = CredentialKey::from_hex(&"22".repeat(32)).unwrap();
        let c = encrypt(&k, "p", "a", "access_token", "x").unwrap();
        assert!(decrypt(&other, "p", "a", "access_token", &c).is_err());
        assert!(decrypt(&k, "p2", "a", "access_token", &c).is_err());
        assert!(decrypt(&k, "p", "a", "refresh_token", &c).is_err());
        let mut b = c.into_bytes();
        *b.last_mut().unwrap() ^= 1;
        assert!(decrypt(
            &k,
            "p",
            "a",
            "access_token",
            std::str::from_utf8(&b).unwrap()
        )
        .is_err());
    }
    #[test]
    fn malformed_and_nonreused_nonce_rejected() {
        let k = key();
        assert!(decrypt(&k, "p", "a", "f", "plaintext").is_err());
        let a = encrypt(&k, "p", "a", "f", "x").unwrap();
        let b = encrypt(&k, "p", "a", "f", "x").unwrap();
        assert_ne!(a, b);
    }
}

#[cfg(test)]
mod binding_tests {
    use super::*;
    #[test]
    fn slash_containing_account_names_cannot_alias_aad() {
        let key = CredentialKey::from_hex(&"11".repeat(32)).unwrap();
        let cipher = encrypt(&key, "a/b", "c", "access_token", "secret").unwrap();
        assert!(decrypt(&key, "a", "b/c", "access_token", &cipher).is_err());
    }
}
