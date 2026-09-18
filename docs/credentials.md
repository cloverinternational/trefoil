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

OpenRouter onboarding uses `TREFOIL_PUBLIC_URL` for its absolute callback URL
(default `http://127.0.0.1:$PORT`, port 8080 when unset). Configure an externally
reachable HTTPS origin when browsers cannot reach the server loopback address.
Headless admins may paste the redirect URL or code to the manual-code endpoint.
The callback state and PKCE verifier are single-use. Other provider-specific
onboarding implementations remain incomplete.

Verify persistence against a disposable database only:
`TREFOIL_TEST_DATABASE_URL=... TREFOIL_CREDENTIAL_KEY=... cargo test --test oauth_credentials -- --ignored`.
The test does not read the production `DATABASE_URL` variable.
