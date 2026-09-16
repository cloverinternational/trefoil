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
  const res = await fetch(`${baseUrl}/v0/management/auth/verify`, { headers });
  const body = await res.json().catch(() => null);
  return { status: res.status, body };
}

function normalize(body: unknown): unknown {
  // No nondeterministic fields in this contract today (no timestamps/ids),
  // but keep a hook here for future endpoints that need normalization
  // before comparison (e.g. request ids, timestamps).
  return body;
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
  body?: unknown
) {
  const headers: Record<string, string> = {};
  if (adminHeader !== undefined) headers['x-admin-key'] = adminHeader;
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  const res = await fetch(`${baseUrl}${path}`, {
    method,
    headers,
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  const json = await res.json().catch(() => null);
  return { status: res.status, body: json };
}

interface KeysCase {
  name: string;
  method: string;
  path: string;
  adminHeader?: string;
  body?: unknown;
}

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

async function runCases(cases: KeysCase[]): Promise<number> {
  let failures = 0;
  for (const testCase of cases) {
    const [plexus, trefoil] = await Promise.all([
      fetchJson(PLEXUS_BASE_URL, testCase.method, testCase.path, testCase.adminHeader, testCase.body),
      fetchJson(TREFOIL_BASE_URL, testCase.method, testCase.path, testCase.adminHeader, testCase.body),
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
  return failures;
}

async function main() {
  let failures = 0;

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

  const total =
    CASES.length +
    KEYS_CASES.length +
    PROVIDERS_CASES.length +
    ALIASES_CASES.length +
    USER_QUOTAS_CASES.length +
    SYSTEM_SETTINGS_CASES.length +
    COOLDOWNS_CASES.length;
  if (failures > 0) {
    console.error(`\n${failures}/${total} case(s) diverged.`);
    process.exit(1);
  }
  console.log(`\nAll ${total} cases match.`);
}

main();
