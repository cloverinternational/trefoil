//! At-rest encryption for provider API keys and inference key secrets,
//! byte-compatible with pinned Plexus `utils/encryption.ts`.
//!
//! `ENCRYPTION_KEY` is 64 hex chars (used directly) or a passphrase derived
//! with scrypt (N=2^14, r=8, p=1, fixed salt). Values are stored as
//! `enc:v1:<iv_hex>:<tag_hex>:<ciphertext_hex>` (AES-256-GCM). Without a key,
//! values are stored as plaintext and startup warns, as in Plexus. Plaintext
//! rows are always readable, so enabling a key later is backwards compatible.
//! OAuth tokens keep their own `TREFOIL_CREDENTIAL_KEY` envelope.
use aes_gcm::{aead::{Aead, KeyInit}, Aes256Gcm, Nonce};
use rand::RngCore;
use std::sync::OnceLock;

const PREFIX: &str = "enc:v1:";
const SCRYPT_SALT: &[u8] = b"plexus-encryption-key-derivation";

fn derive(raw: &str) -> [u8; 32] {
    let mut key = [0u8; 32];
    if raw.len() == 64 && raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        key.copy_from_slice(&hex::decode(raw).expect("validated hex"));
    } else {
        let params = scrypt::Params::new(14, 8, 1, 32).expect("static scrypt params");
        scrypt::scrypt(raw.as_bytes(), SCRYPT_SALT, &params, &mut key).expect("32-byte output");
    }
    key
}

fn key() -> Option<&'static [u8; 32]> {
    static KEY: OnceLock<Option<[u8; 32]>> = OnceLock::new();
    KEY.get_or_init(|| std::env::var("ENCRYPTION_KEY").ok().filter(|v| !v.is_empty()).map(|v| derive(&v))).as_ref()
}

pub fn enabled() -> bool { key().is_some() }

fn encrypt_with(key: &[u8; 32], plaintext: &str) -> String {
    let mut iv = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut iv);
    let mut sealed = Aes256Gcm::new(key.into()).encrypt(Nonce::from_slice(&iv), plaintext.as_bytes()).expect("AES-GCM encryption");
    let tag = sealed.split_off(sealed.len() - 16);
    format!("{PREFIX}{}:{}:{}", hex::encode(iv), hex::encode(tag), hex::encode(sealed))
}

fn decrypt_with(key: Option<&[u8; 32]>, value: &str) -> Result<String, String> {
    let Some(body) = value.strip_prefix(PREFIX) else { return Ok(value.to_owned()) };
    let key = key.ok_or("Cannot decrypt value: ENCRYPTION_KEY is not set but encrypted data was found")?;
    let parts: Vec<_> = body.split(':').collect();
    let [iv, tag, ciphertext] = parts.as_slice() else { return Err("Invalid encrypted value format".into()) };
    let (iv, tag, mut ciphertext) = (hex::decode(iv), hex::decode(tag), hex::decode(ciphertext).map_err(|_| "Invalid encrypted value format")?);
    let (iv, tag) = (iv.map_err(|_| "Invalid encrypted value format")?, tag.map_err(|_| "Invalid encrypted value format")?);
    if iv.len() != 12 || tag.len() != 16 { return Err("Invalid encrypted value format".into()); }
    ciphertext.extend_from_slice(&tag);
    let plain = Aes256Gcm::new(key.into()).decrypt(Nonce::from_slice(&iv), ciphertext.as_slice()).map_err(|_| "Unable to decrypt value")?;
    String::from_utf8(plain).map_err(|_| "Unable to decrypt value".into())
}

/// Encrypts when `ENCRYPTION_KEY` is set; otherwise returns the plaintext.
pub fn encrypt(plaintext: &str) -> String {
    match key() { Some(key) if !plaintext.starts_with(PREFIX) => encrypt_with(key, plaintext), _ => plaintext.to_owned() }
}
pub fn encrypt_opt(value: &Option<String>) -> Option<String> { value.as_deref().map(encrypt) }
/// Plaintext passes through unchanged (backwards compatibility).
pub fn decrypt(value: &str) -> Result<String, String> { decrypt_with(key(), value) }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn envelope_round_trips_and_matches_plexus_format() {
        let key = derive(&"ab".repeat(32));
        let sealed = encrypt_with(&key, "sk-secret");
        let parts: Vec<_> = sealed.strip_prefix(PREFIX).unwrap().split(':').collect();
        assert_eq!((parts[0].len(), parts[1].len(), parts[2].len()), (24, 32, 18));
        assert_eq!(decrypt_with(Some(&key), &sealed).unwrap(), "sk-secret");
        assert_eq!(decrypt_with(None, "plain").unwrap(), "plain");
        assert!(decrypt_with(None, &sealed).unwrap_err().contains("ENCRYPTION_KEY is not set"));
        assert!(decrypt_with(Some(&derive("other passphrase")), &sealed).is_err());
    }
    #[test]
    fn decrypts_ciphertext_produced_by_plexus_node_crypto() {
        // Produced by Plexus encrypt() with ENCRYPTION_KEY="parity passphrase".
        let key = derive("parity passphrase");
        let plexus = "enc:v1:0e57f3f863effbe0edd97e25:64a217c48c5235d6272d110268efe220:b0f2c79510f0f36ce5";
        assert_eq!(decrypt_with(Some(&key), plexus).unwrap(), "sk-parity");
        assert_eq!(decrypt_with(Some(&key), &encrypt_with(&key, "sk-parity")).unwrap(), "sk-parity");
    }
}
