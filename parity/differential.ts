#!/usr/bin/env bun
// Differential parity harness: sends identical requests to a running Plexus
// reference instance and the Rust Trefoil backend, and compares normalized
// responses for the currently ported vertical slice
// (GET /v0/management/auth/verify).
//
// Usage:
//   PLEXUS_BASE_URL=http://127.0.0.1:4100 \
//   TREFOIL_BASE_URL=http://127.0.0.1:8080 \
//   bun run parity/differential.ts
//
// Both servers must already be running. This script sends no real
// credentials or production requests — only the fixed test values below,
// matching packages/backend/src/routes/management/__tests__/admin-auth.test.ts.

const PLEXUS_BASE_URL = process.env.PLEXUS_BASE_URL ?? 'http://127.0.0.1:4100';
const TREFOIL_BASE_URL = process.env.TREFOIL_BASE_URL ?? 'http://127.0.0.1:8080';

// Both servers must be configured with these fixed credentials for the
// comparison to be meaningful:
//   Plexus:  ADMIN_KEY=correct-admin-key, one api key named "test-key" with
//            secret "sk-test-secret"
//   Trefoil: same admin key / key config wired into AppState for this run.
const ADMIN_KEY = 'correct-admin-key';
const LIMITED_KEY_SECRET = 'sk-test-secret';
const REQUEST_TIMEOUT_MS = 15_000;

interface Case {
  name: string;
  header?: string;
}

// Cases mirror admin-auth.test.ts one-for-one.
const CASES: Case[] = [
  { name: 'admin key -> 200 admin principal', header: ADMIN_KEY },
  { name: 'limited key secret -> 200 limited principal', header: LIMITED_KEY_SECRET },
  { name: 'wrong key -> 401', header: 'wrong-key' },
  { name: 'missing header -> 401', header: undefined },
  { name: 'empty header -> 401', header: '' },
];

async function fetchVerify(baseUrl: string, header: string | undefined) {
  const headers: Record<string, string> = {};
  if (header !== undefined) headers['x-admin-key'] = header;
  const res = await fetch(`${baseUrl}/v0/management/auth/verify`, {
    headers,
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
  });
  const body = await res.json().catch(() => null);
  return { status: res.status, body };
}

async function ensureFixture(baseUrl: string) {
  const response = await fetch(`${baseUrl}/v0/management/keys/test-key`, {
    method: 'PUT',
    headers: { 'x-admin-key': ADMIN_KEY, 'Content-Type': 'application/json' },
    body: JSON.stringify({ secret: LIMITED_KEY_SECRET }),
  });
  if (!response.ok) throw new Error(`Fixture provisioning failed at ${baseUrl}: ${response.status}`);
}

function normalize(body: unknown): unknown {
  // Most contracts have no nondeterministic fields (no timestamps/ids). The
  // one exception so far is POST /v0/management/self/rotate, whose response
  // embeds a freshly random secret that will never match between the two
  // servers' independent RNGs -- mask it to a fixed placeholder so the rest
  // of the shape (keyName/message) is still compared structurally.
  if (
    body &&
    typeof body === 'object' &&
    !Array.isArray(body) &&
    'secret' in (body as Record<string, unknown>) &&
    'keyName' in (body as Record<string, unknown>) &&
    'message' in (body as Record<string, unknown>)
  ) {
    const b = body as Record<string, unknown>;
    return { ...b, secret: typeof b.secret === 'string' ? '<secret>' : b.secret };
  }
  return body;
}

function normalizeForCase(path: string, body: unknown): unknown {
  const normalized = normalize(body);
  if (path.startsWith('/v0/management/backup') && normalized && typeof normalized === 'object' && !Array.isArray(normalized)) {
    const object = { ...(normalized as Record<string, unknown>) };
    delete object.created_at;
    // The reference may run against SQLite while Trefoil uses PostgreSQL;
    // the dialect identifies the storage engine, not the backup contract.
    delete object.dialect;
    return object;
  }
  if ((path.includes('/custom-checkers/') || path === '/v0/management/custom-checkers') && normalized && typeof normalized === 'object') {
    const scrub = (value: unknown): unknown => {
      if (Array.isArray(value)) return value.map(scrub);
      if (!value || typeof value !== 'object') return value;
      const object = { ...(value as Record<string, unknown>) };
      delete object.createdAt;
      delete object.updatedAt;
      return object;
    };
    return scrub(normalized);
  }
  if (path.includes('/mcp-servers/') && path.includes('/keys') && normalized && typeof normalized === 'object') {
    const scrub = (value: unknown): unknown => {
      if (Array.isArray(value)) return value.map(scrub);
      if (!value || typeof value !== 'object') return value;
      const object = { ...(value as Record<string, unknown>) };
      if (typeof object.id === 'number') object.id = '<integer>';
      return object;
    };
    return scrub(normalized);
  }
  if (
    path === '/v0/management/config' &&
    normalized &&
    typeof normalized === 'object' &&
    !Array.isArray(normalized)
  ) {
    const object = normalized as Record<string, unknown>;
    const kind = (value: unknown) =>
      Array.isArray(value) ? 'array' : value !== null && typeof value === 'object' ? 'object' : typeof value;
    return Object.fromEntries(
      ['providers', 'models', 'keys', 'failover', 'cooldown', 'timeout', 'stall', 'backgroundExploration', 'mcpOAuth', 'quotas']
        .map((key) => [key, kind(object[key])])
    );
  }
  if (
    path === '/v0/management/config/status' &&
    normalized &&
    typeof normalized === 'object' &&
    !Array.isArray(normalized)
  ) {
    return Object.fromEntries(
      Object.entries(normalized as Record<string, unknown>).map(([key, value]) => [
        key,
        typeof value === 'number' && Number.isInteger(value) ? '<integer>' : value,
      ])
    );
  }
  if (
    path === '/v0/management/config/export' &&
    normalized &&
    typeof normalized === 'object' &&
    !Array.isArray(normalized)
  ) {
    const object = normalized as Record<string, unknown>;
    const kind = (value: unknown) =>
      Array.isArray(value) ? 'array' : value !== null && typeof value === 'object' ? 'object' : typeof value;
    return Object.fromEntries(
      ['providers', 'models', 'keys', 'user_quotas', 'mcp_servers', 'mcp_keys', 'settings', 'oauth_providers']
        .map((key) => [key, kind(object[key])])
    );
  }
  return normalized;
}

// Structural comparison, independent of key insertion order (Fastify/JS
// object literals and Rust serde_json output insertion-order their keys
// differently even when semantically identical).
function deepEqual(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (typeof a !== typeof b) return false;
  if (a === null || b === null) return a === b;
  if (typeof a !== 'object') return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  if (Array.isArray(a) && Array.isArray(b)) {
    if (a.length !== b.length) return false;
    return a.every((item, i) => deepEqual(item, b[i]));
  }
  const aObj = a as Record<string, unknown>;
  const bObj = b as Record<string, unknown>;
  const aKeys = Object.keys(aObj).sort();
  const bKeys = Object.keys(bObj).sort();
  if (aKeys.length !== bKeys.length || aKeys.some((k, i) => k !== bKeys[i])) return false;
  return aKeys.every((k) => deepEqual(aObj[k], bObj[k]));
}

// --- /v0/management/keys/:name CRUD (admin-only) ---

async function fetchJson(
  baseUrl: string,
  method: string,
  path: string,
  adminHeader: string | undefined,
  body?: unknown,
  extraHeaders?: Record<string, string>,
  rawBody?: string
) {
  const headers: Record<string, string> = {};
  if (extraHeaders !== undefined) Object.assign(headers, extraHeaders);
  if (adminHeader !== undefined) headers['x-admin-key'] = adminHeader;
  if (body !== undefined || rawBody !== undefined) headers['Content-Type'] ??= 'application/json';
  const res = await fetch(`${baseUrl}${path}`, {
    method,
    headers,
    body: rawBody ?? (body !== undefined ? JSON.stringify(body) : undefined),
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
  });
  const contentType = res.headers.get('content-type');
  const responseBody = contentType?.includes('text/plain') ? await res.text() : await res.json().catch(() => null);
  return { status: res.status, body: responseBody, contentType };
}

interface KeysCase {
  name: string;
  method: string;
  path: string;
  adminHeader?: string;
  headers?: Record<string, string>;
  body?: unknown;
  rawBody?: string;
}

const BACKUP_CASES: KeysCase[] = [
  { name: 'GET config backup returns envelope for admin', method: 'GET', path: '/v0/management/backup', adminHeader: ADMIN_KEY },
  { name: 'GET config backup rejects limited key', method: 'GET', path: '/v0/management/backup', adminHeader: LIMITED_KEY_SECRET },
  { name: 'GET config backup rejects missing credential', method: 'GET', path: '/v0/management/backup' },
  { name: 'GET full backup returns archive for admin', method: 'GET', path: '/v0/management/backup?full=true', adminHeader: ADMIN_KEY },
  { name: 'POST restore rejects missing credential', method: 'POST', path: '/v0/management/restore', rawBody: '{}', headers: { 'Content-Type': 'application/json' } },
  { name: 'POST restore rejects limited key', method: 'POST', path: '/v0/management/restore', adminHeader: LIMITED_KEY_SECRET, rawBody: '{}', headers: { 'Content-Type': 'application/json' } },
  { name: 'POST restore rejects malformed JSON', method: 'POST', path: '/v0/management/restore', adminHeader: ADMIN_KEY, rawBody: '{', headers: { 'Content-Type': 'application/json' } },
  { name: 'POST restore rejects config without data', method: 'POST', path: '/v0/management/restore', adminHeader: ADMIN_KEY, body: { plexus_backup: true, version: 1 } },
];

// These cases deliberately stop before an upstream provider call.  They make
// the public inference boundary testable in a clean disposable environment
// while still exercising live authentication, validation, and model routing.
// Provider-backed success/streaming cases belong in a run with an explicitly
// configured disposable upstream, not against production credentials.
const OBSERVABILITY_CASES: KeysCase[] = [
  { name: 'GET metrics returns text exposition for admin', method: 'GET', path: '/v0/management/metrics', adminHeader: ADMIN_KEY },
  { name: 'GET metrics rejects limited key', method: 'GET', path: '/v0/management/metrics', adminHeader: LIMITED_KEY_SECRET },
  { name: 'GET metrics rejects missing credential', method: 'GET', path: '/v0/management/metrics' },
];

const INFERENCE_CASES: KeysCase[] = [
  {
    name: 'chat completions rejects missing credentials',
    method: 'POST',
    path: '/v1/chat/completions',
    body: { model: 'missing-provider/missing-model', messages: [] },
  },
  {
    name: 'chat completions rejects missing model',
    method: 'POST',
    path: '/v1/chat/completions',
    adminHeader: ADMIN_KEY,
    body: { messages: [] },
  },
  {
    name: 'chat completions rejects non-array messages',
    method: 'POST',
    path: '/v1/chat/completions',
    adminHeader: ADMIN_KEY,
    body: { model: 'missing-provider/missing-model', messages: {} },
  },
  {
    name: 'chat completions validates streaming requests before dispatch',
    method: 'POST',
    path: '/v1/chat/completions',
    adminHeader: ADMIN_KEY,
    body: { model: 'missing-provider/missing-model', stream: true },
  },
  {
    name: 'embeddings rejects missing input',
    method: 'POST',
    path: '/v1/embeddings',
    adminHeader: ADMIN_KEY,
    body: { model: 'missing-provider/missing-model' },
  },
  {
    name: 'chat completions accepts bearer authentication before routing',
    method: 'POST',
    path: '/v1/chat/completions',
    headers: { authorization: `Bearer ${ADMIN_KEY}` },
    body: { model: 'missing-provider/missing-model', messages: [] },
  },
  {
    name: 'chat completions reports an unknown provider/model',
    method: 'POST',
    path: '/v1/chat/completions',
    adminHeader: ADMIN_KEY,
    body: { model: 'missing-provider/missing-model', messages: [] },
  },
];

// A fresh key name per run avoids collisions with state left over from a
// previous run against the same live Plexus instance.
const runId = Date.now();

const KEYS_CASES: KeysCase[] = [
  {
    name: 'PUT rejects name containing "*"',
    method: 'PUT',
    path: `/v0/management/keys/wild*card-${runId}`,
    adminHeader: ADMIN_KEY,
    body: { secret: 'sk-x' },
  },
  {
    name: 'PUT creates a key, normalizing legacy `quota`',
    method: 'PUT',
    path: `/v0/management/keys/parity-key-${runId}`,
    adminHeader: ADMIN_KEY,
    body: { secret: 'sk-parity', quota: 'legacy-q' },
  },
  {
    name: 'GET returns the created key',
    method: 'GET',
    path: `/v0/management/keys/parity-key-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'PATCH updates comment without touching quotas',
    method: 'PATCH',
    path: `/v0/management/keys/parity-key-${runId}`,
    adminHeader: ADMIN_KEY,
    body: { comment: 'hi' },
  },
  {
    name: 'GET reflects the PATCH',
    method: 'GET',
    path: `/v0/management/keys/parity-key-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'DELETE removes the key',
    method: 'DELETE',
    path: `/v0/management/keys/parity-key-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'GET after delete -> 404',
    method: 'GET',
    path: `/v0/management/keys/parity-key-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'limited key gets 403 on admin-only route',
    method: 'GET',
    path: '/v0/management/keys',
    adminHeader: LIMITED_KEY_SECRET,
  },
  {
    name: 'missing credential gets 401 on admin-only route',
    method: 'GET',
    path: '/v0/management/keys',
  },
];

// --- /v0/management/providers/* CRUD (admin-only) ---

const PROVIDERS_CASES: KeysCase[] = [
  {
    name: 'PUT rejects missing api_key',
    method: 'PUT',
    path: `/v0/management/providers/test-provider-${runId}`,
    adminHeader: ADMIN_KEY,
    body: { api_base_url: 'https://api.example.com' },
  },
  {
    name: 'PUT accepts a valid minimal provider',
    method: 'PUT',
    path: `/v0/management/providers/test-provider-${runId}`,
    adminHeader: ADMIN_KEY,
    body: { api_base_url: 'https://api.example.com', api_key: 'sk-x', adapter: 'openai' },
  },
  {
    name: 'GET returns the created provider with defaults',
    method: 'GET',
    path: `/v0/management/providers/test-provider-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'PUT oauth:// placeholder requires oauth_provider and oauth_account',
    method: 'PUT',
    path: `/v0/management/providers/oauth-test-${runId}`,
    adminHeader: ADMIN_KEY,
    body: { api_base_url: 'oauth://', adapter: 'openai' },
  },
  {
    name: 'PUT rejects an invalid api_base_url',
    method: 'PUT',
    path: `/v0/management/providers/bad-url-${runId}`,
    adminHeader: ADMIN_KEY,
    body: { api_base_url: 'not-a-url', api_key: 'sk-x', adapter: 'openai' },
  },
  {
    name: 'PUT raw_passthrough round-trips with default auth',
    method: 'PUT',
    path: `/v0/management/providers/openrouter-${runId}`,
    adminHeader: ADMIN_KEY,
    body: {
      api_base_url: 'https://openrouter.ai/api/v1',
      api_key: 'sk-x',
      adapter: 'openai',
      raw_passthrough: { enabled: true, base_url: 'https://openrouter.ai/api' },
    },
  },
  {
    name: 'GET reflects raw_passthrough with default auth',
    method: 'GET',
    path: `/v0/management/providers/openrouter-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'PATCH nonexistent provider -> 404',
    method: 'PATCH',
    path: `/v0/management/providers/does-not-exist-${runId}`,
    adminHeader: ADMIN_KEY,
    body: { enabled: false },
  },
  {
    name: 'DELETE returns success with provider name',
    method: 'DELETE',
    path: `/v0/management/providers/test-provider-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'DELETE cleans up the raw_passthrough fixture',
    method: 'DELETE',
    path: `/v0/management/providers/openrouter-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'limited key gets 403 on admin-only providers route',
    method: 'GET',
    path: '/v0/management/providers',
    adminHeader: LIMITED_KEY_SECRET,
  },
  {
    name: 'missing credential gets 401 on admin-only providers route',
    method: 'GET',
    path: '/v0/management/providers',
  },
];

// --- /v0/management/aliases/* and /v0/management/models* CRUD (admin-only) ---

const ALIASES_CASES: KeysCase[] = [
  {
    name: 'PUT creates alias from legacy flat targets',
    method: 'PUT',
    path: `/v0/management/aliases/my-alias-${runId}`,
    adminHeader: ADMIN_KEY,
    body: { targets: [{ provider: 'test-provider', model: 'gpt-4' }] },
  },
  {
    name: 'GET returns the created alias normalized to target_groups',
    method: 'GET',
    path: `/v0/management/aliases/my-alias-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'PATCH merges additional_aliases without touching targets',
    method: 'PATCH',
    path: `/v0/management/aliases/my-alias-${runId}`,
    adminHeader: ADMIN_KEY,
    body: { additional_aliases: ['nick'] },
  },
  {
    name: 'GET reflects the PATCH',
    method: 'GET',
    path: `/v0/management/aliases/my-alias-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'PUT rejects a duplicate target',
    method: 'PUT',
    path: `/v0/management/aliases/dup-alias-${runId}`,
    adminHeader: ADMIN_KEY,
    body: {
      target_groups: [
        {
          name: 'g',
          selector: 'random',
          targets: [
            { provider: 'p', model: 'm' },
            { provider: 'p', model: 'm' },
          ],
        },
      ],
    },
  },
  {
    name: "PUT rejects a target with neither alias nor provider/model",
    method: 'PUT',
    path: `/v0/management/aliases/bad-target-${runId}`,
    adminHeader: ADMIN_KEY,
    body: { target_groups: [{ name: 'g', selector: 'random', targets: [{}] }] },
  },
  {
    name: 'PUT rejects an invalid selector',
    method: 'PUT',
    path: `/v0/management/aliases/no-selector-${runId}`,
    adminHeader: ADMIN_KEY,
    body: {
      target_groups: [
        { name: 'g', selector: 'not-a-selector', targets: [{ provider: 'p', model: 'm' }] },
      ],
    },
  },
  {
    name: 'PUT rejects duplicate additional_aliases',
    method: 'PUT',
    path: `/v0/management/aliases/dup-add-${runId}`,
    adminHeader: ADMIN_KEY,
    body: {
      additional_aliases: ['x', 'x'],
      target_groups: [{ name: 'g', selector: 'random', targets: [{ provider: 'p', model: 'm' }] }],
    },
  },
  {
    name: 'PUT alias-a -> alias-b succeeds',
    method: 'PUT',
    path: `/v0/management/aliases/alias-a-${runId}`,
    adminHeader: ADMIN_KEY,
    body: { target_groups: [{ name: 'g', selector: 'random', targets: [{ alias: `alias-b-${runId}` }] }] },
  },
  {
    name: 'PUT alias-b -> alias-a detects the cycle -> 500',
    method: 'PUT',
    path: `/v0/management/aliases/alias-b-${runId}`,
    adminHeader: ADMIN_KEY,
    body: { target_groups: [{ name: 'g', selector: 'random', targets: [{ alias: `alias-a-${runId}` }] }] },
  },
  {
    name: 'GET alias-b -> 404 (cycle detected before save)',
    method: 'GET',
    path: `/v0/management/aliases/alias-b-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'PATCH nonexistent alias -> 404',
    method: 'PATCH',
    path: `/v0/management/aliases/does-not-exist-${runId}`,
    adminHeader: ADMIN_KEY,
    body: { sticky_session: false },
  },
  {
    name: 'DELETE via /v0/management/models/* removes the alias',
    method: 'DELETE',
    path: `/v0/management/models/my-alias-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'GET after delete -> 404',
    method: 'GET',
    path: `/v0/management/aliases/my-alias-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'DELETE cleans up the alias-a fixture',
    method: 'DELETE',
    path: `/v0/management/models/alias-a-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'limited key gets 403 on admin-only aliases route',
    method: 'GET',
    path: '/v0/management/aliases',
    adminHeader: LIMITED_KEY_SECRET,
  },
  {
    name: 'missing credential gets 401 on admin-only aliases route',
    method: 'GET',
    path: '/v0/management/aliases',
  },
];

// Cases mirror user-quotas.ts routes and user-quotas-delete-conflict.test.ts.
const USER_QUOTAS_CASES: KeysCase[] = [
  {
    name: 'GET nonexistent quota -> 404',
    method: 'GET',
    path: `/v0/management/user-quotas/nope-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'PUT rejects invalid (too-short/uppercase) quota name',
    method: 'PUT',
    path: `/v0/management/user-quotas/A`,
    adminHeader: ADMIN_KEY,
    body: { type: 'daily', limitType: 'requests', limit: 100 },
  },
  {
    name: 'PUT creates a daily quota',
    method: 'PUT',
    path: `/v0/management/user-quotas/my-quota-${runId}`,
    adminHeader: ADMIN_KEY,
    body: { type: 'daily', limitType: 'requests', limit: 100 },
  },
  {
    name: 'GET returns the created quota',
    method: 'GET',
    path: `/v0/management/user-quotas/my-quota-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'PATCH partially updates limit',
    method: 'PATCH',
    path: `/v0/management/user-quotas/my-quota-${runId}`,
    adminHeader: ADMIN_KEY,
    body: { limit: 200 },
  },
  {
    name: 'GET reflects the patched limit',
    method: 'GET',
    path: `/v0/management/user-quotas/my-quota-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'PATCH nonexistent quota -> 404',
    method: 'PATCH',
    path: `/v0/management/user-quotas/does-not-exist-${runId}`,
    adminHeader: ADMIN_KEY,
    body: { limit: 5 },
  },
  {
    name: 'PUT rolling quota missing duration -> 400',
    method: 'PUT',
    path: `/v0/management/user-quotas/rolling-q-${runId}`,
    adminHeader: ADMIN_KEY,
    body: { type: 'rolling', limitType: 'tokens', limit: 50 },
  },
  {
    name: 'PUT quota with scope fields round-trips them',
    method: 'PUT',
    path: `/v0/management/user-quotas/scoped-q-${runId}`,
    adminHeader: ADMIN_KEY,
    body: { type: 'monthly', limitType: 'cost', limit: 10, allowedProviders: ['p1'], warnAt: 0.8, shared: true },
  },
  {
    name: 'GET returns the scoped quota with scope fields',
    method: 'GET',
    path: `/v0/management/user-quotas/scoped-q-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'DELETE unreferenced scoped quota succeeds',
    method: 'DELETE',
    path: `/v0/management/user-quotas/scoped-q-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'DELETE rolling-q fixture cleanup (never created due to validation failure)',
    method: 'DELETE',
    path: `/v0/management/user-quotas/rolling-q-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'PUT a key referencing my-quota',
    method: 'PUT',
    path: `/v0/management/keys/quota-user-key-${runId}`,
    adminHeader: ADMIN_KEY,
    body: { secret: `sk-quota-user-${runId}`, quotas: [`my-quota-${runId}`] },
  },
  {
    name: 'DELETE quota referenced by a key -> 409',
    method: 'DELETE',
    path: `/v0/management/user-quotas/my-quota-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'PATCH system-settings default_quotas references the quota',
    method: 'PATCH',
    path: '/v0/management/system-settings',
    adminHeader: ADMIN_KEY,
    body: { default_quotas: [`my-quota-${runId}`] },
  },
  {
    name: 'DELETE the referencing key so only default_quotas blocks deletion',
    method: 'DELETE',
    path: `/v0/management/keys/quota-user-key-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'DELETE quota referenced by default_quotas -> 409',
    method: 'DELETE',
    path: `/v0/management/user-quotas/my-quota-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'PATCH system-settings clears default_quotas',
    method: 'PATCH',
    path: '/v0/management/system-settings',
    adminHeader: ADMIN_KEY,
    body: { default_quotas: null },
  },
  {
    name: 'DELETE quota now unreferenced succeeds (cleanup)',
    method: 'DELETE',
    path: `/v0/management/user-quotas/my-quota-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'limited key gets 403 on admin-only user-quotas route',
    method: 'GET',
    path: '/v0/management/user-quotas',
    adminHeader: LIMITED_KEY_SECRET,
  },
  {
    name: 'missing credential gets 401 on admin-only user-quotas route',
    method: 'GET',
    path: '/v0/management/user-quotas',
  },
];

// Cases mirror system-settings-default-quotas.test.ts and the PATCH/GET
// contract in config.ts's system-settings routes.
const SYSTEM_SETTINGS_CASES: KeysCase[] = [
  {
    name: 'PATCH default_quotas referencing an unknown quota -> 400',
    method: 'PATCH',
    path: '/v0/management/system-settings',
    adminHeader: ADMIN_KEY,
    body: { default_quotas: [`ghost-quota-${runId}`] },
  },
  {
    name: 'PATCH default_quotas as a non-array -> 400',
    method: 'PATCH',
    path: '/v0/management/system-settings',
    adminHeader: ADMIN_KEY,
    body: { default_quotas: 'not-an-array' },
  },
  {
    name: 'PATCH default_quotas with non-string entries -> 400',
    method: 'PATCH',
    path: '/v0/management/system-settings',
    adminHeader: ADMIN_KEY,
    body: { default_quotas: [1] },
  },
  {
    name: 'PATCH default_quotas empty array succeeds',
    method: 'PATCH',
    path: '/v0/management/system-settings',
    adminHeader: ADMIN_KEY,
    body: { default_quotas: [] },
  },
  {
    name: 'PATCH sets a free-form setting',
    method: 'PATCH',
    path: '/v0/management/system-settings',
    adminHeader: ADMIN_KEY,
    body: { [`custom_setting_${runId}`]: 'whatever' },
  },
  {
    name: 'limited key gets 403 on admin-only system-settings route',
    method: 'GET',
    path: '/v0/management/system-settings',
    adminHeader: LIMITED_KEY_SECRET,
  },
  {
    name: 'missing credential gets 401 on admin-only system-settings route',
    method: 'GET',
    path: '/v0/management/system-settings',
  },
];

// Cases mirror cooldowns.ts. Cooldowns can only be populated by real
// provider failures (no management-API "create" path exists), so these
// cases exercise the empty-list, auth-scoping (GET is readable by any
// valid principal; DELETE is admin-only), and idempotent-delete contracts.
const COOLDOWNS_CASES: KeysCase[] = [
  {
    name: 'GET cooldowns (empty) is readable by admin',
    method: 'GET',
    path: '/v0/management/cooldowns',
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'GET cooldowns (empty) is readable by a limited key',
    method: 'GET',
    path: '/v0/management/cooldowns',
    adminHeader: LIMITED_KEY_SECRET,
  },
  {
    name: 'missing credential gets 401 on cooldowns route',
    method: 'GET',
    path: '/v0/management/cooldowns',
  },
  {
    name: 'wrong credential gets 401 on cooldowns route',
    method: 'GET',
    path: '/v0/management/cooldowns',
    adminHeader: 'wrong-key',
  },
  {
    name: 'DELETE all cooldowns as a limited key -> 403',
    method: 'DELETE',
    path: '/v0/management/cooldowns',
    adminHeader: LIMITED_KEY_SECRET,
  },
  {
    name: 'DELETE all cooldowns as admin succeeds (idempotent on empty state)',
    method: 'DELETE',
    path: '/v0/management/cooldowns',
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'DELETE by provider succeeds (idempotent, no matching entries)',
    method: 'DELETE',
    path: `/v0/management/cooldowns/no-such-provider-${runId}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'DELETE by provider+model succeeds (idempotent, no matching entries)',
    method: 'DELETE',
    path: `/v0/management/cooldowns/no-such-provider-${runId}?model=no-such-model`,
    adminHeader: ADMIN_KEY,
  },
];

// Cases mirror debug.ts (GET/PATCH /v0/management/debug). The global toggle
// and provider/key/alias filter sets are process-wide singleton state (no
// per-run fixture), so every mutating case ends by restoring a known-clean
// state before the next case runs, and the very last case resets
// everything back to defaults so this slice doesn't leak state into a
// later run of the harness against the same live servers.
const DEBUG_CASES: KeysCase[] = [
  {
    name: 'GET debug (clean state) as admin',
    method: 'GET',
    path: '/v0/management/debug',
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'GET debug (clean state) as a limited key',
    method: 'GET',
    path: '/v0/management/debug',
    adminHeader: LIMITED_KEY_SECRET,
  },
  {
    name: 'missing credential gets 401 on debug route',
    method: 'GET',
    path: '/v0/management/debug',
  },
  {
    name: 'PATCH debug as a limited key -> 403',
    method: 'PATCH',
    path: '/v0/management/debug',
    adminHeader: LIMITED_KEY_SECRET,
    body: { enabled: true },
  },
  {
    name: 'missing credential gets 401 on PATCH debug',
    method: 'PATCH',
    path: '/v0/management/debug',
    body: { enabled: true },
  },
  {
    name: 'PATCH debug rejects a non-boolean enabled',
    method: 'PATCH',
    path: '/v0/management/debug',
    adminHeader: ADMIN_KEY,
    body: { enabled: 'yes' },
  },
  {
    name: 'PATCH debug rejects a non-array providers',
    method: 'PATCH',
    path: '/v0/management/debug',
    adminHeader: ADMIN_KEY,
    body: { providers: 'not-an-array' },
  },
  {
    name: 'PATCH debug rejects a non-string entry in keys',
    method: 'PATCH',
    path: '/v0/management/debug',
    adminHeader: ADMIN_KEY,
    body: { keys: [123] },
  },
  {
    name: 'PATCH debug enables the global toggle',
    method: 'PATCH',
    path: '/v0/management/debug',
    adminHeader: ADMIN_KEY,
    body: { enabled: true },
  },
  {
    name: 'GET debug reflects the global toggle as admin',
    method: 'GET',
    path: '/v0/management/debug',
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'GET debug reflects the global toggle for a limited key',
    method: 'GET',
    path: '/v0/management/debug',
    adminHeader: LIMITED_KEY_SECRET,
  },
  {
    name: 'PATCH debug sets providers/keys/aliases filters',
    method: 'PATCH',
    path: '/v0/management/debug',
    adminHeader: ADMIN_KEY,
    body: { providers: [`prov-${runId}`], keys: ['test-key'], aliases: [`alias-${runId}`] },
  },
  {
    name: 'GET debug reflects the filters as admin',
    method: 'GET',
    path: '/v0/management/debug',
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'GET debug reflects the per-key filter for the matching limited key',
    method: 'GET',
    path: '/v0/management/debug',
    adminHeader: LIMITED_KEY_SECRET,
  },
  {
    name: 'PATCH debug clears providers/keys/aliases with explicit null',
    method: 'PATCH',
    path: '/v0/management/debug',
    adminHeader: ADMIN_KEY,
    body: { providers: null, keys: null, aliases: null },
  },
  {
    name: 'GET debug reflects the cleared filters',
    method: 'GET',
    path: '/v0/management/debug',
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'PATCH debug cleanup: disable the global toggle (must run last against this shared state)',
    method: 'PATCH',
    path: '/v0/management/debug',
    adminHeader: ADMIN_KEY,
    body: { enabled: false },
  },
];

const CONFIG_STATUS_CASES: KeysCase[] = [
  {
    name: 'GET config/status returns resource counts for admin',
    method: 'GET',
    path: '/v0/management/config/status',
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'GET config/status rejects limited key',
    method: 'GET',
    path: '/v0/management/config/status',
    adminHeader: LIMITED_KEY_SECRET,
  },
  {
    name: 'GET config/status rejects missing credential',
    method: 'GET',
    path: '/v0/management/config/status',
  },
];

const TIMEOUT_CONFIG_CASES: KeysCase[] = [
  {
    name: 'GET config/timeout returns current timeout',
    method: 'GET', path: '/v0/management/config/timeout', adminHeader: ADMIN_KEY,
  },
  {
    name: 'PATCH config/timeout coerces a numeric string',
    method: 'PATCH', path: '/v0/management/config/timeout', adminHeader: ADMIN_KEY,
    body: { defaultSeconds: '45' },
  },
  {
    name: 'GET config/timeout reflects PATCH',
    method: 'GET', path: '/v0/management/config/timeout', adminHeader: ADMIN_KEY,
  },
  {
    name: 'PATCH config/timeout rejects a fractional value',
    method: 'PATCH', path: '/v0/management/config/timeout', adminHeader: ADMIN_KEY,
    body: { defaultSeconds: 1.5 },
  },
  {
    name: 'PATCH config/timeout rejects null',
    method: 'PATCH', path: '/v0/management/config/timeout', adminHeader: ADMIN_KEY,
    body: { defaultSeconds: null },
  },
  {
    name: 'GET config/timeout rejects limited key',
    method: 'GET', path: '/v0/management/config/timeout', adminHeader: LIMITED_KEY_SECRET,
  },
  {
    name: 'GET config/timeout rejects missing credential',
    method: 'GET', path: '/v0/management/config/timeout',
  },
  {
    name: 'PATCH config/timeout cleanup restores default',
    method: 'PATCH', path: '/v0/management/config/timeout', adminHeader: ADMIN_KEY,
    body: { defaultSeconds: 300 },
  },
];

const MCP_ENABLED_CASES: KeysCase[] = [
  { name: 'GET config/mcp-enabled returns current value', method: 'GET', path: '/v0/management/config/mcp-enabled', adminHeader: ADMIN_KEY },
  { name: 'PATCH config/mcp-enabled disables MCP', method: 'PATCH', path: '/v0/management/config/mcp-enabled', adminHeader: ADMIN_KEY, body: { enabled: false } },
  { name: 'GET config/mcp-enabled reflects PATCH', method: 'GET', path: '/v0/management/config/mcp-enabled', adminHeader: ADMIN_KEY },
  { name: 'PATCH config/mcp-enabled rejects string', method: 'PATCH', path: '/v0/management/config/mcp-enabled', adminHeader: ADMIN_KEY, body: { enabled: 'false' } },
  { name: 'PATCH config/mcp-enabled rejects missing field', method: 'PATCH', path: '/v0/management/config/mcp-enabled', adminHeader: ADMIN_KEY, body: {} },
  { name: 'GET config/mcp-enabled rejects limited key', method: 'GET', path: '/v0/management/config/mcp-enabled', adminHeader: LIMITED_KEY_SECRET },
  { name: 'PATCH config/mcp-enabled cleanup restores true', method: 'PATCH', path: '/v0/management/config/mcp-enabled', adminHeader: ADMIN_KEY, body: { enabled: true } },
];

const EXPLORATION_RATE_CASES: KeysCase[] = [
  { name: 'GET config/exploration-rate returns current values', method: 'GET', path: '/v0/management/config/exploration-rate', adminHeader: ADMIN_KEY },
  { name: 'PATCH config/exploration-rate applies JS numeric coercions', method: 'PATCH', path: '/v0/management/config/exploration-rate', adminHeader: ADMIN_KEY, body: { performanceExplorationRate: '0.2', latencyExplorationRate: true, e2ePerformanceExplorationRate: null } },
  { name: 'GET config/exploration-rate reflects PATCH', method: 'GET', path: '/v0/management/config/exploration-rate', adminHeader: ADMIN_KEY },
  { name: 'PATCH config/exploration-rate rejects out-of-range value', method: 'PATCH', path: '/v0/management/config/exploration-rate', adminHeader: ADMIN_KEY, body: { performanceExplorationRate: 1.1 } },
  { name: 'PATCH config/exploration-rate rejects non-object body', method: 'PATCH', path: '/v0/management/config/exploration-rate', adminHeader: ADMIN_KEY, body: [] },
  { name: 'GET config/exploration-rate rejects limited key', method: 'GET', path: '/v0/management/config/exploration-rate', adminHeader: LIMITED_KEY_SECRET },
  { name: 'PATCH config/exploration-rate cleanup restores defaults', method: 'PATCH', path: '/v0/management/config/exploration-rate', adminHeader: ADMIN_KEY, body: { performanceExplorationRate: 0.05, latencyExplorationRate: 0.05, e2ePerformanceExplorationRate: 0.05 } },
];

const FAILOVER_COOLDOWN_CASES: KeysCase[] = [
  { name: 'GET config/failover returns policy', method: 'GET', path: '/v0/management/config/failover', adminHeader: ADMIN_KEY },
  { name: 'PATCH config/failover disables failover', method: 'PATCH', path: '/v0/management/config/failover', adminHeader: ADMIN_KEY, body: { enabled: false } },
  { name: 'GET config/failover reflects PATCH', method: 'GET', path: '/v0/management/config/failover', adminHeader: ADMIN_KEY },
  { name: 'PATCH config/failover rejects non-object body', method: 'PATCH', path: '/v0/management/config/failover', adminHeader: ADMIN_KEY, body: [] },
  { name: 'GET config/failover rejects limited key', method: 'GET', path: '/v0/management/config/failover', adminHeader: LIMITED_KEY_SECRET },
  { name: 'PATCH config/failover cleanup restores enabled', method: 'PATCH', path: '/v0/management/config/failover', adminHeader: ADMIN_KEY, body: { enabled: true } },
  { name: 'GET config/cooldown returns policy', method: 'GET', path: '/v0/management/config/cooldown', adminHeader: ADMIN_KEY },
  { name: 'PATCH config/cooldown applies numeric coercion', method: 'PATCH', path: '/v0/management/config/cooldown', adminHeader: ADMIN_KEY, body: { initialMinutes: '0.5', maxMinutes: true } },
  { name: 'GET config/cooldown reflects PATCH', method: 'GET', path: '/v0/management/config/cooldown', adminHeader: ADMIN_KEY },
  { name: 'PATCH config/cooldown rejects null', method: 'PATCH', path: '/v0/management/config/cooldown', adminHeader: ADMIN_KEY, body: { initialMinutes: null } },
  { name: 'GET config/cooldown rejects limited key', method: 'GET', path: '/v0/management/config/cooldown', adminHeader: LIMITED_KEY_SECRET },
  { name: 'PATCH config/cooldown cleanup restores defaults', method: 'PATCH', path: '/v0/management/config/cooldown', adminHeader: ADMIN_KEY, body: { initialMinutes: 2, maxMinutes: 300 } },
];

const CAPTURE_TRACE_CASES: KeysCase[] = [
  { name: 'GET capture-trace-on-error returns current value', method: 'GET', path: '/v0/management/config/capture-trace-on-error', adminHeader: ADMIN_KEY },
  { name: 'PATCH capture-trace-on-error enables capture', method: 'PATCH', path: '/v0/management/config/capture-trace-on-error', adminHeader: ADMIN_KEY, body: { enabled: true } },
  { name: 'GET capture-trace-on-error reflects PATCH', method: 'GET', path: '/v0/management/config/capture-trace-on-error', adminHeader: ADMIN_KEY },
  { name: 'PATCH capture-trace-on-error rejects string', method: 'PATCH', path: '/v0/management/config/capture-trace-on-error', adminHeader: ADMIN_KEY, body: { enabled: 'true' } },
  { name: 'PATCH capture-trace-on-error rejects missing field', method: 'PATCH', path: '/v0/management/config/capture-trace-on-error', adminHeader: ADMIN_KEY, body: {} },
  { name: 'GET capture-trace-on-error rejects limited key', method: 'GET', path: '/v0/management/config/capture-trace-on-error', adminHeader: LIMITED_KEY_SECRET },
  { name: 'PATCH capture-trace-on-error cleanup restores false', method: 'PATCH', path: '/v0/management/config/capture-trace-on-error', adminHeader: ADMIN_KEY, body: { enabled: false } },
];

const STALL_CONFIG_CASES: KeysCase[] = [
  { name: 'GET config/stall returns all defaults', method: 'GET', path: '/v0/management/config/stall', adminHeader: ADMIN_KEY },
  { name: 'PATCH config/stall updates all fields', method: 'PATCH', path: '/v0/management/config/stall', adminHeader: ADMIN_KEY, body: { ttfbSeconds: '12.5', ttfbBytes: '250', minBytesPerSecond: 500, windowSeconds: 5, gracePeriodSeconds: 0, stallCooldown: true } },
  { name: 'GET config/stall reflects PATCH', method: 'GET', path: '/v0/management/config/stall', adminHeader: ADMIN_KEY },
  { name: 'PATCH config/stall rejects fractional ttfbBytes', method: 'PATCH', path: '/v0/management/config/stall', adminHeader: ADMIN_KEY, body: { ttfbBytes: 50.5 } },
  { name: 'PATCH config/stall rejects invalid stallCooldown', method: 'PATCH', path: '/v0/management/config/stall', adminHeader: ADMIN_KEY, body: { stallCooldown: 'true' } },
  { name: 'PATCH config/stall accepts nullable thresholds', method: 'PATCH', path: '/v0/management/config/stall', adminHeader: ADMIN_KEY, body: { ttfbSeconds: null, minBytesPerSecond: null } },
  { name: 'GET config/stall rejects limited key', method: 'GET', path: '/v0/management/config/stall', adminHeader: LIMITED_KEY_SECRET },
  { name: 'PATCH config/stall cleanup restores defaults', method: 'PATCH', path: '/v0/management/config/stall', adminHeader: ADMIN_KEY, body: { ttfbSeconds: null, ttfbBytes: 100, minBytesPerSecond: null, windowSeconds: 10, gracePeriodSeconds: 30, stallCooldown: false } },
];

const VISION_FALLTHROUGH_CASES: KeysCase[] = [
  { name: 'GET config/vision-fallthrough returns current object', method: 'GET', path: '/v0/management/config/vision-fallthrough', adminHeader: ADMIN_KEY },
  { name: 'PATCH config/vision-fallthrough empty object is a no-op', method: 'PATCH', path: '/v0/management/config/vision-fallthrough', adminHeader: ADMIN_KEY, body: {} },
  { name: 'PATCH config/vision-fallthrough rejects non-object body', method: 'PATCH', path: '/v0/management/config/vision-fallthrough', adminHeader: ADMIN_KEY, body: [] },
  { name: 'GET config/vision-fallthrough rejects limited key', method: 'GET', path: '/v0/management/config/vision-fallthrough', adminHeader: LIMITED_KEY_SECRET },
];

const LOGGING_CASES: KeysCase[] = [
  { name: 'GET logging/level returns runtime state', method: 'GET', path: '/v0/management/logging/level', adminHeader: ADMIN_KEY },
  { name: 'PUT logging/level trims and normalizes', method: 'PUT', path: '/v0/management/logging/level', adminHeader: ADMIN_KEY, body: { level: ' DEBUG ' } },
  { name: 'GET logging/level reflects PUT', method: 'GET', path: '/v0/management/logging/level', adminHeader: ADMIN_KEY },
  { name: 'PUT logging/level rejects unsupported trace', method: 'PUT', path: '/v0/management/logging/level', adminHeader: ADMIN_KEY, body: { level: 'trace' } },
  { name: 'GET logging/level rejects limited key', method: 'GET', path: '/v0/management/logging/level', adminHeader: LIMITED_KEY_SECRET },
  { name: 'DELETE logging/level resets startup default', method: 'DELETE', path: '/v0/management/logging/level', adminHeader: ADMIN_KEY },
  { name: 'GET logging/modules starts empty', method: 'GET', path: '/v0/management/logging/modules', adminHeader: ADMIN_KEY },
  { name: 'PUT logging/modules deduplicates entries', method: 'PUT', path: '/v0/management/logging/modules', adminHeader: ADMIN_KEY, body: { modules: ['routing', 'routing', 'quota'] } },
  { name: 'GET logging/modules reflects PUT', method: 'GET', path: '/v0/management/logging/modules', adminHeader: ADMIN_KEY },
  { name: 'PUT logging/modules rejects empty module', method: 'PUT', path: '/v0/management/logging/modules', adminHeader: ADMIN_KEY, body: { modules: [''] } },
  { name: 'GET logging/modules rejects missing credential', method: 'GET', path: '/v0/management/logging/modules' },
  { name: 'DELETE logging/modules clears filter', method: 'DELETE', path: '/v0/management/logging/modules', adminHeader: ADMIN_KEY },
];

const BACKGROUND_EXPLORATION_CASES: KeysCase[] = [
  { name: 'GET background-exploration returns defaults', method: 'GET', path: '/v0/management/config/background-exploration', adminHeader: ADMIN_KEY },
  { name: 'PATCH background-exploration applies values and coercion', method: 'PATCH', path: '/v0/management/config/background-exploration', adminHeader: ADMIN_KEY, body: { enabled: true, stalenessThresholdSeconds: '120', workerConcurrency: 4 } },
  { name: 'GET background-exploration reflects PATCH', method: 'GET', path: '/v0/management/config/background-exploration', adminHeader: ADMIN_KEY },
  { name: 'PATCH background-exploration rejects invalid enabled', method: 'PATCH', path: '/v0/management/config/background-exploration', adminHeader: ADMIN_KEY, body: { enabled: 'false' } },
  { name: 'PATCH background-exploration rejects invalid concurrency', method: 'PATCH', path: '/v0/management/config/background-exploration', adminHeader: ADMIN_KEY, body: { workerConcurrency: 17 } },
  { name: 'GET background-exploration rejects limited key', method: 'GET', path: '/v0/management/config/background-exploration', adminHeader: LIMITED_KEY_SECRET },
  { name: 'PATCH background-exploration cleanup restores defaults', method: 'PATCH', path: '/v0/management/config/background-exploration', adminHeader: ADMIN_KEY, body: { enabled: false, stalenessThresholdSeconds: 600, workerConcurrency: 2 } },
];

const TRUSTED_PROXIES_CASES: KeysCase[] = [
  { name: 'GET trusted-proxies returns defaults', method: 'GET', path: '/v0/management/config/trusted-proxies', adminHeader: ADMIN_KEY },
  { name: 'PATCH trusted-proxies normalizes valid rules', method: 'PATCH', path: '/v0/management/config/trusted-proxies', adminHeader: ADMIN_KEY, body: { trustedProxies: [' 127.0.0.1 ', '', '2001:db8::/32'] } },
  { name: 'GET trusted-proxies reflects PATCH', method: 'GET', path: '/v0/management/config/trusted-proxies', adminHeader: ADMIN_KEY },
  { name: 'PATCH trusted-proxies rejects invalid rule', method: 'PATCH', path: '/v0/management/config/trusted-proxies', adminHeader: ADMIN_KEY, body: { trustedProxies: ['bad'] } },
  { name: 'PATCH trusted-proxies rejects wrong shape', method: 'PATCH', path: '/v0/management/config/trusted-proxies', adminHeader: ADMIN_KEY, body: { trustedProxies: 'x' } },
  { name: 'GET trusted-proxies rejects limited key', method: 'GET', path: '/v0/management/config/trusted-proxies', adminHeader: LIMITED_KEY_SECRET },
  { name: 'PATCH trusted-proxies cleanup restores defaults', method: 'PATCH', path: '/v0/management/config/trusted-proxies', adminHeader: ADMIN_KEY, body: { trustedProxies: ['0.0.0.0/0', '::/0'] } },
];

const ERRORS_CASES: KeysCase[] = [
  { name: 'GET errors returns scoped page', method: 'GET', path: '/v0/management/errors?limit=5&offset=0', adminHeader: ADMIN_KEY },
  { name: 'GET errors rejects missing credential', method: 'GET', path: '/v0/management/errors' },
  { name: 'DELETE all errors rejects limited key', method: 'DELETE', path: '/v0/management/errors', adminHeader: LIMITED_KEY_SECRET },
  { name: 'DELETE missing error is idempotent for admin', method: 'DELETE', path: '/v0/management/errors/parity-missing-error', adminHeader: ADMIN_KEY },
  { name: 'DELETE missing error is hidden from limited key', method: 'DELETE', path: '/v0/management/errors/parity-missing-error', adminHeader: LIMITED_KEY_SECRET },
];

const PERFORMANCE_CASES: KeysCase[] = [
  { name: 'GET performance returns current aggregates', method: 'GET', path: '/v0/management/performance', adminHeader: ADMIN_KEY },
  { name: 'GET performance accepts provider/model filters', method: 'GET', path: '/v0/management/performance?provider=missing&model=missing', adminHeader: ADMIN_KEY },
  { name: 'GET performance rejects limited key', method: 'GET', path: '/v0/management/performance', adminHeader: LIMITED_KEY_SECRET },
  { name: 'DELETE performance requires model parameter', method: 'DELETE', path: '/v0/management/performance', adminHeader: ADMIN_KEY },
  { name: 'DELETE nonexistent performance model is idempotent', method: 'DELETE', path: '/v0/management/performance?model=parity-missing-performance', adminHeader: ADMIN_KEY },
];

const CONFIG_SNAPSHOT_CASES: KeysCase[] = [
  { name: 'GET config returns structured runtime snapshot', method: 'GET', path: '/v0/management/config', adminHeader: ADMIN_KEY },
  { name: 'GET config rejects limited key', method: 'GET', path: '/v0/management/config', adminHeader: LIMITED_KEY_SECRET },
  { name: 'GET config rejects missing credential', method: 'GET', path: '/v0/management/config' },
];

const CONFIG_EXPORT_CASES: KeysCase[] = [
  { name: 'GET config/export returns raw repository collections', method: 'GET', path: '/v0/management/config/export', adminHeader: ADMIN_KEY },
  { name: 'GET config/export rejects limited key', method: 'GET', path: '/v0/management/config/export', adminHeader: LIMITED_KEY_SECRET },
  { name: 'GET config/export rejects missing credential', method: 'GET', path: '/v0/management/config/export' },
];

const VISION_CACHE_CASES: KeysCase[] = [
  { name: 'POST vision descriptor cache clear succeeds', method: 'POST', path: '/v0/management/cache/vision-descriptor/clear', adminHeader: ADMIN_KEY },
  { name: 'POST vision descriptor cache clear rejects limited key', method: 'POST', path: '/v0/management/cache/vision-descriptor/clear', adminHeader: LIMITED_KEY_SECRET },
  { name: 'POST vision descriptor cache clear rejects missing credential', method: 'POST', path: '/v0/management/cache/vision-descriptor/clear' },
];

const OAUTH_PROVIDER_CASES: KeysCase[] = [
  { name: 'GET oauth/providers lists pinned provider descriptors', method: 'GET', path: '/v0/management/oauth/providers', adminHeader: ADMIN_KEY },
  { name: 'GET oauth/providers rejects limited key', method: 'GET', path: '/v0/management/oauth/providers', adminHeader: LIMITED_KEY_SECRET },
  { name: 'GET oauth/providers rejects missing credential', method: 'GET', path: '/v0/management/oauth/providers' },
];

const QUOTA_CHECKER_TYPE_CASES: KeysCase[] = [
  { name: 'GET quota-checker-types lists pinned registry', method: 'GET', path: '/v0/management/quota-checker-types', adminHeader: ADMIN_KEY },
  { name: 'GET quota-checker-types rejects limited key', method: 'GET', path: '/v0/management/quota-checker-types', adminHeader: LIMITED_KEY_SECRET },
  { name: 'GET quota-checker-types rejects missing credential', method: 'GET', path: '/v0/management/quota-checker-types' },
];

const CONCURRENCY_CASES: KeysCase[] = [
  { name: 'GET concurrency live returns current snapshot', method: 'GET', path: '/v0/management/concurrency', adminHeader: ADMIN_KEY },
  { name: 'GET concurrency historical provider timeline', method: 'GET', path: '/v0/management/concurrency?mode=timeline&timeRange=custom&startDate=1970-01-01&endDate=1970-01-02&groupBy=provider', adminHeader: ADMIN_KEY },
  { name: 'GET concurrency historical model timeline', method: 'GET', path: '/v0/management/concurrency?mode=timeline&timeRange=custom&startDate=1970-01-01&endDate=1970-01-02&groupBy=model', adminHeader: ADMIN_KEY },
  { name: 'GET concurrency rejects invalid custom dates', method: 'GET', path: '/v0/management/concurrency?mode=timeline&timeRange=custom&startDate=bad&endDate=worse', adminHeader: ADMIN_KEY },
  { name: 'GET concurrency rejects limited key', method: 'GET', path: '/v0/management/concurrency', adminHeader: LIMITED_KEY_SECRET },
  { name: 'GET concurrency rejects missing credential', method: 'GET', path: '/v0/management/concurrency' },
];

const PI_PROVIDER_CASES: KeysCase[] = [
  { name: 'GET pi/providers lists pinned provider registry', method: 'GET', path: '/v0/management/pi/providers', adminHeader: ADMIN_KEY },
  { name: 'GET pi/providers rejects limited key', method: 'GET', path: '/v0/management/pi/providers', adminHeader: LIMITED_KEY_SECRET },
  { name: 'GET pi/providers rejects missing credential', method: 'GET', path: '/v0/management/pi/providers' },
];

const OAUTH_STATUS_CASES: KeysCase[] = [
  { name: 'GET oauth credential status returns false when absent', method: 'GET', path: '/v0/management/oauth/credentials/status?providerId=anthropic&accountId=parity-missing-account', adminHeader: ADMIN_KEY },
  { name: 'GET oauth credential status validates missing query', method: 'GET', path: '/v0/management/oauth/credentials/status', adminHeader: ADMIN_KEY },
  { name: 'GET oauth credential status validates empty account', method: 'GET', path: '/v0/management/oauth/credentials/status?providerId=anthropic&accountId=', adminHeader: ADMIN_KEY },
  { name: 'GET oauth credential status rejects limited key', method: 'GET', path: '/v0/management/oauth/credentials/status?providerId=anthropic&accountId=x', adminHeader: LIMITED_KEY_SECRET },
  { name: 'GET oauth credential status rejects missing credential', method: 'GET', path: '/v0/management/oauth/credentials/status?providerId=anthropic&accountId=x' },
];

const OAUTH_DELETE_CASES: KeysCase[] = [
  { name: 'DELETE missing OAuth credential returns 404', method: 'DELETE', path: '/v0/management/oauth/credentials', adminHeader: ADMIN_KEY, body: { providerId: 'anthropic', accountId: 'parity-missing-account' } },
  { name: 'DELETE OAuth credential validates missing body fields', method: 'DELETE', path: '/v0/management/oauth/credentials', adminHeader: ADMIN_KEY, body: {} },
  { name: 'DELETE OAuth credential validates empty account', method: 'DELETE', path: '/v0/management/oauth/credentials', adminHeader: ADMIN_KEY, body: { providerId: 'anthropic', accountId: '' } },
  { name: 'DELETE OAuth credential rejects limited key', method: 'DELETE', path: '/v0/management/oauth/credentials', adminHeader: LIMITED_KEY_SECRET, body: { providerId: 'anthropic', accountId: 'x' } },
  { name: 'DELETE OAuth credential rejects missing credential', method: 'DELETE', path: '/v0/management/oauth/credentials', body: { providerId: 'anthropic', accountId: 'x' } },
];

const COMPACTION_CASES: KeysCase[] = [
  { name: 'GET config/compaction returns current object', method: 'GET', path: '/v0/management/config/compaction', adminHeader: ADMIN_KEY },
  { name: 'PATCH config/compaction stores valid config', method: 'PATCH', path: '/v0/management/config/compaction', adminHeader: ADMIN_KEY, body: { enabled: true, strategy: 'headroom', triggerRatio: 0.7, minTokens: 2000, headroom: { baseUrl: 'http://localhost:8787', timeoutMs: 5000 } } },
  { name: 'PATCH config/compaction nested merge preserves fields', method: 'PATCH', path: '/v0/management/config/compaction', adminHeader: ADMIN_KEY, body: { headroom: { timeoutMs: 1000 } } },
  { name: 'PATCH config/compaction null clears scalar and nested field', method: 'PATCH', path: '/v0/management/config/compaction', adminHeader: ADMIN_KEY, body: { triggerRatio: null, headroom: { timeoutMs: null } } },
  { name: 'PATCH config/compaction rejects invalid ratio', method: 'PATCH', path: '/v0/management/config/compaction', adminHeader: ADMIN_KEY, body: { triggerRatio: 2 } },
  { name: 'PATCH config/compaction rejects non-object body', method: 'PATCH', path: '/v0/management/config/compaction', adminHeader: ADMIN_KEY, body: [] },
  { name: 'GET config/compaction rejects limited key', method: 'GET', path: '/v0/management/config/compaction', adminHeader: LIMITED_KEY_SECRET },
  { name: 'PATCH config/compaction cleanup clears fields', method: 'PATCH', path: '/v0/management/config/compaction', adminHeader: ADMIN_KEY, body: { enabled: null, strategy: null, minTokens: null, native: null, headroom: null } },
];

const QUOTA_DISCOVERY_CASES: KeysCase[] = [
  { name: 'GET quota-checkers returns known types and no configured checkers', method: 'GET', path: '/v0/management/quota-checkers', adminHeader: ADMIN_KEY },
  { name: 'GET quota-checkers rejects limited key', method: 'GET', path: '/v0/management/quota-checkers', adminHeader: LIMITED_KEY_SECRET },
  { name: 'GET quotas returns no scheduler results', method: 'GET', path: '/v0/management/quotas', adminHeader: ADMIN_KEY },
  { name: 'GET quotas rejects missing credential', method: 'GET', path: '/v0/management/quotas' },
];

const QUOTA_CLEAR_CASES: KeysCase[] = [
  { name: 'POST quota/clear requires key', method: 'POST', path: '/v0/management/quota/clear', adminHeader: ADMIN_KEY, body: {} },
  { name: 'POST quota/clear returns 404 for missing key with quota', method: 'POST', path: '/v0/management/quota/clear', adminHeader: ADMIN_KEY, body: { key: 'parity-missing-key', quota: 'q' } },
  { name: 'POST quota/clear rejects unattached quota', method: 'POST', path: '/v0/management/quota/clear', adminHeader: ADMIN_KEY, body: { key: 'test-key', quota: 'parity-unattached' } },
  { name: 'POST quota/clear all is idempotent for arbitrary key', method: 'POST', path: '/v0/management/quota/clear', adminHeader: ADMIN_KEY, body: { key: 'parity-no-state' } },
  { name: 'POST quota/clear rejects limited key', method: 'POST', path: '/v0/management/quota/clear', adminHeader: LIMITED_KEY_SECRET, body: { key: 'x' } },
];

const CUSTOM_CHECKER_ID = `parity-checker-${runId}`;
const CUSTOM_CHECKER_CODE = "return [{ key: provider, used: options.used, status: 'ok' }];";
const CUSTOM_CHECKER_CASES: KeysCase[] = [
  { name: 'PUT custom checker creates fixture', method: 'PUT', path: `/v0/management/custom-checkers/${CUSTOM_CHECKER_ID}`, adminHeader: ADMIN_KEY, body: { displayName: 'Parity checker', code: CUSTOM_CHECKER_CODE, enabled: true } },
  { name: 'GET custom checker returns persisted definition', method: 'GET', path: `/v0/management/custom-checkers/${CUSTOM_CHECKER_ID}`, adminHeader: ADMIN_KEY },
  { name: 'PATCH custom checker preserves omitted fields', method: 'PATCH', path: `/v0/management/custom-checkers/${CUSTOM_CHECKER_ID}`, adminHeader: ADMIN_KEY, body: { enabled: false } },
  { name: 'POST custom checker test returns meters', method: 'POST', path: `/v0/management/custom-checkers/${CUSTOM_CHECKER_ID}/test`, adminHeader: ADMIN_KEY, body: { provider: 'parity-provider', options: { used: 42 }, code: CUSTOM_CHECKER_CODE } },
  { name: 'POST custom checker check rejects disabled fixture', method: 'POST', path: `/v0/management/custom-checkers/${CUSTOM_CHECKER_ID}/check`, adminHeader: ADMIN_KEY, body: { provider: 'parity-provider', options: { used: 42 } } },
  { name: 'PUT custom checker rejects malformed code', method: 'PUT', path: `/v0/management/custom-checkers/${CUSTOM_CHECKER_ID}-bad`, adminHeader: ADMIN_KEY, body: { displayName: 'Bad', code: "return require('fs');" } },
  { name: 'GET custom checker rejects limited key', method: 'GET', path: `/v0/management/custom-checkers/${CUSTOM_CHECKER_ID}`, adminHeader: LIMITED_KEY_SECRET },
  { name: 'DELETE custom checker removes fixture', method: 'DELETE', path: `/v0/management/custom-checkers/${CUSTOM_CHECKER_ID}`, adminHeader: ADMIN_KEY },
  { name: 'GET deleted custom checker returns 404', method: 'GET', path: `/v0/management/custom-checkers/${CUSTOM_CHECKER_ID}`, adminHeader: ADMIN_KEY },
];

const MCP_SERVER_NAME = `parity-mcp-${runId}`;
const MCP_CASES: KeysCase[] = [
  { name: 'PUT MCP server creates fixture', method: 'PUT', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}`, adminHeader: ADMIN_KEY, body: { upstream_url: 'https://example.invalid/mcp', enabled: true, mode: 'remote_http', auth_scheme: 'bearer' } },
  { name: 'GET MCP server returns persisted configuration', method: 'GET', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}`, adminHeader: ADMIN_KEY },
  { name: 'PATCH MCP server preserves omitted fields', method: 'PATCH', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}`, adminHeader: ADMIN_KEY, body: { enabled: false } },
  { name: 'POST MCP key creates access key', method: 'POST', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}/keys`, adminHeader: ADMIN_KEY, body: { key: `parity-mcp-key-${runId}`, is_active: true } },
  { name: 'GET MCP keys lists persisted access key', method: 'GET', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}/keys`, adminHeader: ADMIN_KEY },
  { name: 'PUT MCP server rejects invalid mode', method: 'PUT', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}-bad`, adminHeader: ADMIN_KEY, body: { upstream_url: 'https://example.invalid/mcp', mode: 'invalid' } },
  { name: 'GET missing MCP server returns 404', method: 'GET', path: '/v0/management/mcp-servers/parity-missing-server', adminHeader: ADMIN_KEY },
  { name: 'GET MCP server rejects limited key', method: 'GET', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}`, adminHeader: LIMITED_KEY_SECRET },
  { name: 'DELETE MCP server removes fixture', method: 'DELETE', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}`, adminHeader: ADMIN_KEY },
  { name: 'GET deleted MCP server returns 404', method: 'GET', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}`, adminHeader: ADMIN_KEY },
];

const DEBUG_LOG_CASES: KeysCase[] = [
  { name: 'GET debug/logs returns scoped page', method: 'GET', path: '/v0/management/debug/logs?limit=5&offset=0', adminHeader: ADMIN_KEY },
  { name: 'GET missing debug log returns 404', method: 'GET', path: '/v0/management/debug/logs/parity-missing-log', adminHeader: ADMIN_KEY },
  { name: 'DELETE all debug logs rejects limited key', method: 'DELETE', path: '/v0/management/debug/logs', adminHeader: LIMITED_KEY_SECRET },
  { name: 'DELETE missing debug log is idempotent for admin', method: 'DELETE', path: '/v0/management/debug/logs/parity-missing-log', adminHeader: ADMIN_KEY },
  { name: 'DELETE missing debug log is hidden from limited key', method: 'DELETE', path: '/v0/management/debug/logs/parity-missing-log', adminHeader: LIMITED_KEY_SECRET },
  { name: 'GET debug/logs rejects missing credential', method: 'GET', path: '/v0/management/debug/logs' },
];

const MCP_LOG_CASES: KeysCase[] = [
  { name: 'GET mcp-logs returns paginated list', method: 'GET', path: '/v0/management/mcp-logs?limit=5&offset=0', adminHeader: ADMIN_KEY },
  { name: 'GET mcp-logs accepts filters', method: 'GET', path: '/v0/management/mcp-logs?serverName=missing&apiKey=missing', adminHeader: ADMIN_KEY },
  { name: 'GET mcp-logs rejects limited key', method: 'GET', path: '/v0/management/mcp-logs', adminHeader: LIMITED_KEY_SECRET },
  { name: 'DELETE all mcp-logs rejects limited key', method: 'DELETE', path: '/v0/management/mcp-logs', adminHeader: LIMITED_KEY_SECRET },
  { name: 'DELETE missing mcp-log is idempotent for admin', method: 'DELETE', path: '/v0/management/mcp-logs/parity-missing-log', adminHeader: ADMIN_KEY },
  { name: 'DELETE mcp-log rejects missing credential', method: 'DELETE', path: '/v0/management/mcp-logs/x' },
];

// Cases mirror self.ts. A dedicated key fixture is used throughout instead
// of the shared `test-key`/LIMITED_KEY_SECRET fixture, because
// self/rotate mutates the credential in place -- reusing the shared
// fixture would silently break every later case (and every later slice)
// that relies on LIMITED_KEY_SECRET still matching. self/rotate is
// exercised exactly once, as the final case against this fixture, and its
// randomly-generated `secret` field is masked by `normalize()` before
// comparison since the two servers' RNGs will never agree on a value.
const SELF_TARGET_SECRET = `sk-self-target-${runId}`;
const SELF_GHOST_KEY_NAME = `self-ghost-${runId}`;

const SELF_CASES: KeysCase[] = [
  {
    name: 'PUT creates the self-service target key fixture',
    method: 'PUT',
    path: `/v0/management/keys/self-target-${runId}`,
    adminHeader: ADMIN_KEY,
    body: { secret: SELF_TARGET_SECRET },
  },
  {
    name: 'GET self/me as the target key returns limited metadata',
    method: 'GET',
    path: '/v0/management/self/me',
    adminHeader: SELF_TARGET_SECRET,
  },
  {
    name: 'GET self/me as admin returns {role: admin}',
    method: 'GET',
    path: '/v0/management/self/me',
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'missing credential gets 401 on self/me',
    method: 'GET',
    path: '/v0/management/self/me',
  },
  {
    name: 'PATCH self/comment sets the comment',
    method: 'PATCH',
    path: '/v0/management/self/comment',
    adminHeader: SELF_TARGET_SECRET,
    body: { comment: 'hello from parity' },
  },
  {
    name: 'PATCH self/comment with no comment field clears it',
    method: 'PATCH',
    path: '/v0/management/self/comment',
    adminHeader: SELF_TARGET_SECRET,
    body: {},
  },
  {
    name: 'PATCH self/comment rejects a non-string comment',
    method: 'PATCH',
    path: '/v0/management/self/comment',
    adminHeader: SELF_TARGET_SECRET,
    body: { comment: 123 },
  },
  {
    name: 'PATCH self/comment as admin without ?keyName= -> 400',
    method: 'PATCH',
    path: '/v0/management/self/comment',
    adminHeader: ADMIN_KEY,
    body: { comment: 'x' },
  },
  {
    name: 'PATCH self/comment as admin for a nonexistent key -> 404',
    method: 'PATCH',
    path: `/v0/management/self/comment?keyName=${SELF_GHOST_KEY_NAME}`,
    adminHeader: ADMIN_KEY,
    body: { comment: 'x' },
  },
  {
    name: 'missing credential gets 401 on self/comment',
    method: 'PATCH',
    path: '/v0/management/self/comment',
    body: { comment: 'x' },
  },
  {
    name: 'POST self/debug/toggle enables trace for the target key',
    method: 'POST',
    path: '/v0/management/self/debug/toggle',
    adminHeader: SELF_TARGET_SECRET,
    body: { enabled: true },
  },
  {
    name: 'POST self/debug/toggle disables trace for the target key',
    method: 'POST',
    path: '/v0/management/self/debug/toggle',
    adminHeader: SELF_TARGET_SECRET,
    body: { enabled: false },
  },
  {
    name: 'POST self/debug/toggle rejects a non-boolean enabled',
    method: 'POST',
    path: '/v0/management/self/debug/toggle',
    adminHeader: SELF_TARGET_SECRET,
    body: { enabled: 'yes' },
  },
  {
    name: 'POST self/debug/toggle as admin for a nonexistent key still succeeds (no existence check)',
    method: 'POST',
    path: `/v0/management/self/debug/toggle?keyName=${SELF_GHOST_KEY_NAME}`,
    adminHeader: ADMIN_KEY,
    body: { enabled: true },
  },
  {
    name: 'POST self/debug/toggle cleanup: disable the nonexistent-key entry',
    method: 'POST',
    path: `/v0/management/self/debug/toggle?keyName=${SELF_GHOST_KEY_NAME}`,
    adminHeader: ADMIN_KEY,
    body: { enabled: false },
  },
  {
    name: 'missing credential gets 401 on self/debug/toggle',
    method: 'POST',
    path: '/v0/management/self/debug/toggle',
    body: { enabled: true },
  },
  {
    name: 'GET self/quota as the target key returns the no-quota-assigned shape',
    method: 'GET',
    path: '/v0/management/self/quota',
    adminHeader: SELF_TARGET_SECRET,
  },
  {
    name: 'GET self/quota as admin without ?keyName= -> 400',
    method: 'GET',
    path: '/v0/management/self/quota',
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'GET self/quota as admin for a nonexistent key -> 404',
    method: 'GET',
    path: `/v0/management/self/quota?keyName=${SELF_GHOST_KEY_NAME}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'missing credential gets 401 on self/quota',
    method: 'GET',
    path: '/v0/management/self/quota',
  },
  {
    name: 'POST self/rotate as admin without ?keyName= -> 400',
    method: 'POST',
    path: '/v0/management/self/rotate',
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'POST self/rotate as admin for a nonexistent key -> 404',
    method: 'POST',
    path: `/v0/management/self/rotate?keyName=${SELF_GHOST_KEY_NAME}`,
    adminHeader: ADMIN_KEY,
  },
  {
    name: 'missing credential gets 401 on self/rotate',
    method: 'POST',
    path: '/v0/management/self/rotate',
  },
  {
    name: 'POST self/rotate as the target key replaces its own secret (must run last against this fixture)',
    method: 'POST',
    path: '/v0/management/self/rotate',
    adminHeader: SELF_TARGET_SECRET,
  },
  {
    name: 'DELETE cleans up the self-service target key fixture',
    method: 'DELETE',
    path: `/v0/management/keys/self-target-${runId}`,
    adminHeader: ADMIN_KEY,
  },
];

async function runCases(cases: KeysCase[]): Promise<number> {
  let failures = 0;
  for (const testCase of cases) {
    const [plexus, trefoil] = await Promise.all([
      fetchJson(PLEXUS_BASE_URL, testCase.method, testCase.path, testCase.adminHeader, testCase.body, testCase.headers, testCase.rawBody),
      fetchJson(TREFOIL_BASE_URL, testCase.method, testCase.path, testCase.adminHeader, testCase.body, testCase.headers, testCase.rawBody),
    ]);

    const statusMatch = plexus.status === trefoil.status;
    const contentTypeMatch =
      (testCase.path.startsWith('/v0/management/backup') || testCase.path === '/v0/management/restore' || testCase.path === '/v0/management/metrics')
      ? plexus.contentType === trefoil.contentType
      : true;
    const bodyMatch = deepEqual(
      normalizeForCase(testCase.path, plexus.body),
      normalizeForCase(testCase.path, trefoil.body)
    );
    const pass = statusMatch && bodyMatch && contentTypeMatch;
    if (!pass) failures += 1;

    console.log(`${pass ? 'PASS' : 'FAIL'} ${testCase.name}`);
    if (!pass) {
      console.log(`  plexus:  ${plexus.status} ${JSON.stringify(plexus.body)}`);
      console.log(`  trefoil: ${trefoil.status} ${JSON.stringify(trefoil.body)}`);
    }
  }
  return failures;
}

async function main() {
  let failures = 0;
  await Promise.all([ensureFixture(PLEXUS_BASE_URL), ensureFixture(TREFOIL_BASE_URL)]);

  // Plexus rebuilds its in-memory key cache asynchronously after management
  // writes. Wait until the fixture is visible through the endpoint under test
  // before comparing the rest of the suite; otherwise every limited-key case
  // is reported as a parity failure even though provisioning succeeded.
  for (const baseUrl of [PLEXUS_BASE_URL, TREFOIL_BASE_URL]) {
    for (let attempt = 0; attempt < 40; attempt++) {
      const response = await fetch(`${baseUrl}/v0/management/auth/verify`, {
        headers: { 'x-admin-key': LIMITED_KEY_SECRET },
      });
      if (response.status === 200) break;
      if (attempt === 39) throw new Error(`Fixture key did not become visible at ${baseUrl}`);
      await new Promise((resolve) => setTimeout(resolve, 25));
    }
  }

  for (const testCase of CASES) {
    const [plexus, trefoil] = await Promise.all([
      fetchVerify(PLEXUS_BASE_URL, testCase.header),
      fetchVerify(TREFOIL_BASE_URL, testCase.header),
    ]);

    const statusMatch = plexus.status === trefoil.status;
    const bodyMatch = deepEqual(normalize(plexus.body), normalize(trefoil.body));
    const pass = statusMatch && bodyMatch;
    if (!pass) failures += 1;

    console.log(`${pass ? 'PASS' : 'FAIL'} ${testCase.name}`);
    if (!pass) {
      console.log(`  plexus:  ${plexus.status} ${JSON.stringify(plexus.body)}`);
      console.log(`  trefoil: ${trefoil.status} ${JSON.stringify(trefoil.body)}`);
    }
  }

  failures += await runCases(KEYS_CASES);
  failures += await runCases(PROVIDERS_CASES);
  failures += await runCases(ALIASES_CASES);
  failures += await runCases(USER_QUOTAS_CASES);
  failures += await runCases(SYSTEM_SETTINGS_CASES);
  failures += await runCases(COOLDOWNS_CASES);
  failures += await runCases(DEBUG_CASES);
  failures += await runCases(CONFIG_STATUS_CASES);
  failures += await runCases(TIMEOUT_CONFIG_CASES);
  failures += await runCases(MCP_ENABLED_CASES);
  failures += await runCases(EXPLORATION_RATE_CASES);
  failures += await runCases(FAILOVER_COOLDOWN_CASES);
  failures += await runCases(CAPTURE_TRACE_CASES);
  failures += await runCases(STALL_CONFIG_CASES);
  failures += await runCases(VISION_FALLTHROUGH_CASES);
  failures += await runCases(LOGGING_CASES);
  failures += await runCases(BACKGROUND_EXPLORATION_CASES);
  failures += await runCases(TRUSTED_PROXIES_CASES);
  failures += await runCases(ERRORS_CASES);
  failures += await runCases(PERFORMANCE_CASES);
  failures += await runCases(CONFIG_SNAPSHOT_CASES);
  failures += await runCases(CONFIG_EXPORT_CASES);
  failures += await runCases(BACKUP_CASES);
  failures += await runCases(OBSERVABILITY_CASES);
  failures += await runCases(CONCURRENCY_CASES);
  failures += await runCases(VISION_CACHE_CASES);
  failures += await runCases(OAUTH_PROVIDER_CASES);
  failures += await runCases(QUOTA_CHECKER_TYPE_CASES);
  failures += await runCases(PI_PROVIDER_CASES);
  failures += await runCases(OAUTH_STATUS_CASES);
  failures += await runCases(OAUTH_DELETE_CASES);
  failures += await runCases(COMPACTION_CASES);
  failures += await runCases(QUOTA_DISCOVERY_CASES);
  failures += await runCases(QUOTA_CLEAR_CASES);
  failures += await runCases(CUSTOM_CHECKER_CASES);
  failures += await runCases(MCP_CASES);
  failures += await runCases(DEBUG_LOG_CASES);
  failures += await runCases(MCP_LOG_CASES);
  failures += await runCases(INFERENCE_CASES);

  // Plexus's ConfigService coalesces config-mutating writes into an
  // in-memory cache rebuild ~100ms after the write (see COALESCE_MS in
  // config-service.ts). The self-service routes read that cache, so a key
  // created immediately before self/* calls can 401 on Plexus until the
  // rebuild fires. Poll the key back via the ordinary keys GET (which reads
  // the same cache) before exercising the self-service cases against it.
  const [selfSetupCase, ...selfRestCases] = SELF_CASES;
  failures += await runCases([selfSetupCase]);
  // Poll self/me itself (not the admin keys CRUD GET, which reads straight
  // from the repo and would 200 well before ConfigService's cache catches
  // up) with the target key's own secret, since that's the code path the
  // rest of SELF_CASES actually exercises.
  for (let attempt = 0; attempt < 20; attempt++) {
    const res = await fetch(`${PLEXUS_BASE_URL}/v0/management/self/me`, {
      headers: { 'x-admin-key': SELF_TARGET_SECRET },
    });
    if (res.status === 200) break;
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  failures += await runCases(selfRestCases);

  const total =
    CASES.length +
    KEYS_CASES.length +
    PROVIDERS_CASES.length +
    ALIASES_CASES.length +
    USER_QUOTAS_CASES.length +
    SYSTEM_SETTINGS_CASES.length +
    COOLDOWNS_CASES.length +
    DEBUG_CASES.length +
    CONFIG_STATUS_CASES.length +
    TIMEOUT_CONFIG_CASES.length +
    MCP_ENABLED_CASES.length +
    EXPLORATION_RATE_CASES.length +
    FAILOVER_COOLDOWN_CASES.length +
    CAPTURE_TRACE_CASES.length +
    STALL_CONFIG_CASES.length +
    VISION_FALLTHROUGH_CASES.length +
    LOGGING_CASES.length +
    BACKGROUND_EXPLORATION_CASES.length +
    TRUSTED_PROXIES_CASES.length +
    ERRORS_CASES.length +
    PERFORMANCE_CASES.length +
    CONFIG_SNAPSHOT_CASES.length +
    CONFIG_EXPORT_CASES.length +
    BACKUP_CASES.length +
    OBSERVABILITY_CASES.length +
    CONCURRENCY_CASES.length +
    VISION_CACHE_CASES.length +
    OAUTH_PROVIDER_CASES.length +
    QUOTA_CHECKER_TYPE_CASES.length +
    PI_PROVIDER_CASES.length +
    OAUTH_STATUS_CASES.length +
    OAUTH_DELETE_CASES.length +
    COMPACTION_CASES.length +
    QUOTA_DISCOVERY_CASES.length +
    QUOTA_CLEAR_CASES.length +
    CUSTOM_CHECKER_CASES.length +
    MCP_CASES.length +
    DEBUG_LOG_CASES.length +
    MCP_LOG_CASES.length +
    INFERENCE_CASES.length +
    SELF_CASES.length;
  if (failures > 0) {
    console.error(`\n${failures}/${total} case(s) diverged.`);
    process.exit(1);
  }
  console.log(`\nAll ${total} cases match.`);
}

main();
