# OAuth credential encryption

OAuth access and refresh tokens are encrypted at rest with AES-256-GCM. Set
`TREFOIL_CREDENTIAL_KEY` to exactly 64 hexadecimal characters (32 bytes),
preferably generated with `openssl rand -hex 32`. The key is never stored in
PostgreSQL and must be supplied to every process that reads or writes OAuth
credentials.

The provider, account, and token field are authenticated as associated data.
Ciphertexts use the versioned `v1:` envelope and random nonces. Missing or
invalid keys, corrupted ciphertext, wrong keys, and legacy plaintext rows fail
closed; plaintext rows are not automatically migrated. Back up the key
securely. Key rotation currently requires an explicit application migration
that decrypts with the old key and re-encrypts with the new key; changing the
environment value alone makes existing credentials unavailable.
