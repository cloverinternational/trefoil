# Plexus → Trefoil parity: working gap list

Oracle: `parity/oracle/` (stack.sh, oauth_stack.sh, oracle.py, mock_upstream.py, scenarios/).
Reference pin: `.reference/plexus` @ `a4cd39b0`.

## Status (2026-10-29)
- Full differential `parity/run.sh`: **428/428 match** at the new pin (no new
  OpenAPI-driven cases; the first run at the moved pin showed 4 divergences, fixed below). The harness drops Plexus's nondeterministic
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
- Pin moved to `a4cd39b0` (46 commits, no new PostgreSQL migrations). Ported #1006
  (pi-ai bump: see accepted differences), #1012, #1018, #1019, #1021, #1022, #1024,
  #1025, #1026, #1028, #1033, #1035, #1037, #1039, #1040 and the Cheaper Inference and
  Runcrate presets; #1014, #1016 and #1023 are N/A (Bun-compiled workers, no Synthetic
  checker, frontend-only).
- The moved pin exposed three Trefoil gaps, fixed: `oauth/providers` reported
  `usesCallbackServer: false` for `openai` (#1040); `GET backup` failed without
  `TREFOIL_CREDENTIAL_KEY` once OAuth credentials were exported; and dispatch resolved
  models only from the embedded snapshot, so Anthropic OAuth routes for a model that
  exists only in the models store/overlay sent `max_tokens` 64000 instead of the
  catalog limit (#1026). Dispatch now resolves through the merged catalog.
- Ported upstream #1001: strict JSON-schema `response_format` is forwarded across
  formats.
- Ported upstream #999: alias metadata `short_name` override with a derived default.
- Upstream #1000 (free-text alias targets) is N/A for Trefoil.

## Known, accepted differences
- The embedded pi-ai catalog snapshots (`assets/pi-catalog.json`,
  `engine/dispatch/pi_models.json`) are still pi-ai 1.2.0 while Plexus is on 1.4.0
  (#1006). Models added since only resolve once the catalog store or pi.dev refresh
  supplies them. Regenerating both snapshots is a separate task.
- Stateful suites can show one-off diffs straight after `--setup` (observed:
  `models list`, `decisions usage rows`); a fresh stack, `--setup` alone, a few
  seconds' wait, then a single run was 0 differences for every suite except the
  documented quota/network timing and wording ones.
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
