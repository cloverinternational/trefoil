# Plexus → Trefoil parity: working gap list

Oracle: `parity/oracle/` (stack.sh, oauth_stack.sh, oracle.py, mock_upstream.py, scenarios/).
Reference pin: `.reference/plexus` @ `17b7e608`.

## Status (2026-10-18)
- Full differential `parity/run.sh`: **428/428 match** at the new pin (two consecutive
  runs; no new OpenAPI-driven cases). The harness drops Plexus's nondeterministic
  "Failed to insert pending usage record" log entries (a Plexus UsageStorage
  upsert/pending-insert race, seen on SQLite and on PostgreSQL via
  `PLEXUS_STORAGE=postgres`) from the system-logs case; all other log entries are
  compared exactly, so a startup network failure on one side (model-metadata or
  codex-version fetch) shows up as a one-off `plexus_system_logs/recent` divergence.
- Oracle (scenarios that differ / total): inference-basic 0/19, inference-matrix 0/227,
  models 0/69, media 0/58, decisions 0/10, dispatcher-features 0/20,
  observability 0/61, oauth-dispatch 0/28 (on `oauth_stack.sh`, with
  `ORACLE_MASK_HEADERS=1` and `ORACLE_MASK_REGEX` masking `cch=`, the 64-hex device
  id and UUID session ids), quota-enforcement 2/16 (`resetsAt` ms timing),
  network-errors 2/5 (Bun DNS-cache wording, below).
  - Stateful suites (quota-enforcement, observability, dispatcher-features) only
    compare cleanly on a fresh `stack.sh up`: run `--setup` alone
    (`--only <no match>`), give Plexus a few seconds to reload its config, then
    run once. A run straight after `--setup` can show transient
    Plexus-not-yet-reloaded diffs; re-running a stateful suite then compares
    state the two sides accumulated differently.
  - `models ui` embeds the process-start `created` time; it matches only because
    the oracle masks `created` and differs byte-for-byte otherwise.
- All inference routes (chat, messages, responses incl. lite, gemini, completions,
  embeddings, images, speech, transcriptions, decisions/systemone, /v1/models) run on
  the engine port (`backend/src/engine/`); legacy inference.rs removed.

## Resolved since last pass
- Ported upstream #1001: strict JSON-schema `response_format` is forwarded across
  formats.
- Ported upstream #999: alias metadata `short_name` override with a derived default.
- Upstream #1000 (free-text alias targets) is N/A for Trefoil.

## Known, accepted differences
- Error stacks stored as first line only (no JS stack frames).
- Network errors now use Bun fetch's messages and codes (`ConnectionRefused`, ECONNRESET),
  so retryability matches Plexus (`scenarios/network-errors.json`). Bun's DNS-cache
  wording ("Was there a typo in the url or port?" on a repeat lookup) varies even
  inside Plexus and is not emulated.
- OAuth token refresh paths reuse Trefoil refresh code (token-endpoint error wording);
  live OAuth/provider requests not exercised (no account authorization).
- Three Plexus bugs deliberately not copied: stalled requests never leave in-flight
  list; debug flag left on after disconnect during token estimation; and
  `withDefaultShortName` mutating the cached catalog object, so a later alias with a
  `name` override but no `short_name` override can pick up a catalog-derived
  `short_name` depending on resolve order. Trefoil always derives the default
  `short_name` from the merged name.
- `oauth_login_sessions` table unused (sessions in memory like Plexus).
