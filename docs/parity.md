# Parity with Plexus

Trefoil aims to be a drop-in replacement for
[Plexus](https://github.com/mcowger/plexus): the same inference endpoints, the
same `/v0` management API, the same PostgreSQL schema lineage, and the same
observable behaviour toward clients and upstream providers. Parity is verified
by running both gateways side by side and diffing what they do, not by
reading code.

## Reference revision

All parity work targets one pinned Plexus commit:

```
a4cd39b078ddd2daadc04f108cc039a9c8805716
```

Behaviour that changed in Plexus after this revision is out of scope until the
pin is moved deliberately (update this file, `.github/workflows/parity.yml`,
and re-run every suite below).

## Setting up the reference checkout

The reference lives at `.reference/plexus`, which is gitignored and never
committed. It needs [Bun](https://bun.sh).

```sh
git clone https://github.com/mcowger/plexus.git .reference/plexus
git -C .reference/plexus checkout a4cd39b078ddd2daadc04f108cc039a9c8805716
(cd .reference/plexus && bun install --frozen-lockfile --ignore-scripts)
# Needed by the differential runner unless SKIP_REFERENCE_BUILD=1:
(cd .reference/plexus && bun run build:frontend && bun run generate:openapi:asset)
```

`--ignore-scripts` keeps dependency lifecycle scripts from running on your
machine. A few Rust tests read fixtures from the reference checkout and some
corpus tests are `#[ignore]`d unless a corpus generated from it is supplied
(`PROVIDER_SCHEMA_CORPUS`, `ALIAS_SCHEMA_CORPUS`).

## How parity is checked

### 1. Differential HTTP harness (`parity/run.sh`)

`parity/run.sh` starts a disposable PostgreSQL container, builds and starts
Plexus and Trefoil, and runs `parity/differential.ts`. Each named case sends
the same request to both gateways and compares the status code and the JSON
body structurally. The suite covers the management API (keys, providers,
aliases, quotas, cooldowns, config, backup/restore, logging, debug, MCP
servers, OAuth sessions), the inference boundary (authentication, validation,
unknown models/providers), and the MCP management endpoint. The run writes a
machine-readable report and refuses to report success if any registered case
was not executed.

```sh
REFERENCE_DIR=.reference/plexus parity/run.sh
```

### 2. Oracle suites (`parity/oracle/`)

The oracle compares what each gateway sends *upstream*, not just what it
returns. `parity/oracle/stack.sh up` starts a long-lived Plexus/Trefoil pair,
each pointed at its own recording mock upstream (`mock_upstream.py`).
`parity/oracle/oracle.py` replays a scenario file against both gateways and
diffs, after masking volatile IDs and timestamps:

- the client-visible response (status, headers of interest, body or SSE
  stream), and
- every request the gateway made to the upstream (path, headers, body).

```sh
parity/oracle/stack.sh up
python3 parity/oracle/oracle.py parity/oracle/scenarios/inference-basic.json --setup
parity/oracle/stack.sh down
```

Scenario suites in `parity/oracle/scenarios/`:

| Suite | Covers |
|---|---|
| `inference-basic.json`, `inference-matrix.json` | Chat Completions, Anthropic Messages, Responses, Gemini and legacy Completions across input/output API translations, streaming and non-streaming |
| `media.json` | Embeddings, image generation/edits, speech, transcriptions |
| `decisions.json` | `/v1/decisions` |
| `models.json` | `/v1/models` and model metadata |
| `dispatcher-features.json` | Routing, failover, retries and dispatcher options |
| `quota-enforcement.json` | Key and user quota enforcement |
| `observability.json` | Usage records, debug logs, errors, performance |
| `network-errors.json` | Upstream connection failures and retryability |
| `oauth-dispatch.json` | OAuth-backed providers (run with `oauth_stack.sh`, fake credentials and a mock-redirected catalog) |

`oracle.py` options: `--only SUBSTR` runs matching scenarios, `--verbose`
prints full diffs; `ORACLE_MASK_HEADERS`, `ORACLE_EXTRA_VOLATILE` and
`ORACLE_MASK_REGEX` widen the masking for suites with inherently volatile
values. No real provider is contacted.

### 3. Focused end-to-end scripts (`parity/*-e2e.sh`)

Trefoil-only scripts that build the backend, start it against a throwaway
PostgreSQL container, and exercise one area over HTTP (alias safeguards,
usage-log filters, MCP system logs, model metadata, OAuth descriptors and
`ENCRYPTION_KEY` at rest, the pi-ai catalog, provider contracts and ETags,
quota registry, and OAuth credential storage). See `parity/README.md` for the
list.

### 4. Unit and frontend tests

`cargo test --workspace --lib` holds deterministic unit tests for the ported
logic (transformers, routing, quotas, OAuth refresh, MCP stdio framing, backup
archive validation). The frontend's Vitest suite (`npm test`) covers the API
adapter and helpers; Playwright specs in `frontend/e2e/` cover the UI.

### CI

The regular CI workflow (`.github/workflows/ci.yml`) runs formatting, Clippy,
unit tests, the frontend build and the Docker builds. The full differential
run is in `.github/workflows/parity.yml` and is started manually
(`workflow_dispatch`) because it builds both gateways and installs Bun and
Playwright.

## Known, accepted differences

These are the deliberate or unavoidable differences that the suites accept
(tracked in detail in `parity/oracle/GAPS.md`):

- **Error stacks.** Stored error details contain only the first line; there are
  no JavaScript stack frames to reproduce.
- **Network error wording.** Trefoil reproduces Bun fetch's error messages and
  codes (for example `ConnectionRefused`, `ECONNRESET`) so retry decisions match,
  but not Bun's variable DNS-cache wording on repeated lookups.
- **OAuth token refresh.** Refresh paths use Trefoil's own refresh code, so
  token-endpoint error wording can differ. Live OAuth exchanges with real
  providers are not part of the automated suites.
- **Plexus bugs not copied.** Stalled requests leave the in-flight list in
  Trefoil, and the debug flag is not left enabled after a client disconnects
  during token estimation.
- **OAuth login sessions** are held in memory, as in Plexus; the
  `oauth_login_sessions` table exists for schema compatibility but is unused.
- **Storage ordering.** A few observability reads differ in null ordering
  between Plexus's SQLite storage and Trefoil's PostgreSQL storage.
- **Timing.** Values derived from wall-clock time (for example quota
  `resetsAt` in milliseconds) can differ by the time between the two requests.
- **Reference nondeterminism.** The harness drops Plexus's intermittent
  "Failed to insert pending usage record" log entries (a race in the
  reference's usage storage) from the system-logs comparison; all other log
  entries are compared exactly.

Trefoil also contains features Plexus does not have (catalog organization
groups/tags, saved analytics dashboards). Their schema lives in Trefoil-only
migrations (see `backend/migrations/README.md`); they are additive and are not
part of the parity claim.

## Migrating from Plexus

`migration/plexus_to_trefoil.py` copies configuration (OAuth credentials,
providers, models, aliases, API keys, user quotas, MCP servers, system
settings) and optionally history (`--with-history`) from a Plexus SQLite
database into a Trefoil PostgreSQL database. It decrypts Plexus-encrypted
values with Plexus's `ENCRYPTION_KEY` and re-encrypts OAuth tokens with
`TREFOIL_CREDENTIAL_KEY`. Run it against a copy of `plexus.db` taken while
Plexus is stopped; read the script's header for prerequisites and options.
Treat any SQL it writes with `--out` as a secret.
