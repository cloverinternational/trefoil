# Security policy

## Supported versions

Trefoil is pre-1.0. Security fixes are made on the latest release and `main`
only.

| Version | Supported |
|---|---|
| 0.1.x | yes |

## Reporting a vulnerability

Please do **not** open a public issue for security problems.

Report vulnerabilities privately through GitHub's
[private vulnerability reporting](https://github.com/cloverinternational/trefoil/security/advisories/new)
for this repository. Include:

- the affected version or commit,
- a description of the issue and its impact,
- steps to reproduce or a proof of concept, and
- any suggested fix.

We aim to acknowledge reports within 5 business days and to agree on a
disclosure timeline with you once the issue is confirmed. Please give us a
reasonable chance to release a fix before disclosing publicly.

Issues in upstream Plexus behaviour that Trefoil reproduces may also affect
[Plexus](https://github.com/mcowger/plexus); we will coordinate with its
maintainers where appropriate.

## Handling secrets

Trefoil relies on three operator-supplied secrets. Generate each one with a
CSPRNG, for example `openssl rand -hex 32`, store them outside version control
(`.env` with mode `600`, or your platform's secret store), and back them up
separately from the database.

| Secret | What it protects | If it leaks | If it is lost |
|---|---|---|---|
| `ADMIN_KEY` | Full control of the gateway through the `/v0` management API and admin UI, including reading provider configuration (provider API keys are returned decrypted to admins, as in Plexus; they are encrypted at rest when `ENCRYPTION_KEY` is set) and creating API keys. | Rotate it immediately (change the value and restart the backend) and review API keys, providers and MCP servers for changes. | Set a new value and restart; nothing is encrypted with it. |
| `TREFOIL_CREDENTIAL_KEY` | OAuth access and refresh tokens in `oauth_credentials` (AES-256-GCM; provider, account and field are bound as associated data). | Treat all stored OAuth tokens as compromised: revoke them at each provider and sign in again. | Stored OAuth credentials cannot be decrypted; every OAuth account must be re-authorized. |
| `ENCRYPTION_KEY` | Provider API keys, API key secrets and other sensitive settings at rest (`enc:v1:` envelope, AES-256-GCM). | Rotate the provider API keys and Trefoil API keys it protected. | Encrypted values cannot be decrypted; re-enter provider keys and re-issue API keys. |

Notes:

- Without `ENCRYPTION_KEY`, provider API keys and key secrets are stored in
  PostgreSQL in plaintext and the backend logs a warning at startup. When it is
  set, existing plaintext provider API keys and API key secrets are encrypted
  on the next start.
- Changing `TREFOIL_CREDENTIAL_KEY` or `ENCRYPTION_KEY` does not re-encrypt
  existing data. There is no built-in rotation.
- Database dumps and backup exports from `/v0/management/backup` can contain
  credentials. Protect them like the keys themselves.
- The bundled Compose stack serves HTTP. Terminate TLS in front of it before
  exposing it to any untrusted network, and never publish PostgreSQL.
- Never paste real credentials into issues, logs or test fixtures. The parity
  and end-to-end scripts in `parity/` use fake credentials and disposable
  databases only.
