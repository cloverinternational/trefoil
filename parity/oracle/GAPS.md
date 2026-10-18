# Plexus → Trefoil parity: working gap list

Oracle: `parity/oracle/` (stack.sh, oauth_stack.sh, oracle.py, mock_upstream.py, scenarios/).

## Status (2026-10-16)
- Full differential `parity/run.sh`: **428/428 match** (two consecutive runs). The
  harness drops Plexus's nondeterministic "Failed to insert pending usage record"
  log entries (a Plexus UsageStorage upsert/pending-insert race, seen on SQLite and
  on PostgreSQL via `PLEXUS_STORAGE=postgres`) from the system-logs case; all other
  log entries are compared exactly.
- Oracle: inference-basic 0/19, inference-matrix 0/211, media 0/58, decisions 0/10,
  oauth-dispatch 0/28, models 22/23, dispatcher-features 19/20, quota-enforcement 14/16
  (resetsAt ms timing), observability 50/61 (SQLite null ordering, JS stack traces).
- All inference routes (chat, messages, responses incl. lite, gemini, completions,
  embeddings, images, speech, transcriptions, decisions/systemone, /v1/models) run on
  the engine port (`backend/src/engine/`); legacy inference.rs removed.

## Resolved since last pass
- o200k_base tokenizer (tiktoken-rs, counts checked against js-tiktoken) installed
  at startup; special-token text falls back to the heuristic like js-tiktoken's throw.
- DebugManager capture wired for media + decisions (debug rows 0/55 diff);
  debug-log request headers carry Plexus's normalized `authorization`.

## Known, accepted differences
- Error stacks stored as first line only (no JS stack frames).
- Network errors now use Bun fetch's messages and codes (`ConnectionRefused`, ECONNRESET),
  so retryability matches Plexus (`scenarios/network-errors.json`). Bun's DNS-cache
  wording ("Was there a typo in the url or port?" on a repeat lookup) varies even
  inside Plexus and is not emulated.
- OAuth token refresh paths reuse Trefoil refresh code (token-endpoint error wording);
  live OAuth/provider requests not exercised (no account authorization).
- Two Plexus bugs deliberately not copied: stalled requests never leave in-flight
  list; debug flag left on after disconnect during token estimation.
- `oauth_login_sessions` table unused (sessions in memory like Plexus).
