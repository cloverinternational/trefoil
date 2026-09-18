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

import { gunzipSync } from 'node:zlib';

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
const EXPECTED_CASE_COUNT = 407;
const REPORT_PATH = process.env.PARITY_REPORT ?? 'target/differential-report.json';

type CaseStatus = 'pass' | 'fail' | 'error';
export interface CaseResult {
  name: string;
  status: CaseStatus;
  plexus_status?: number;
  trefoil_status?: number;
  error?: string;
}
const caseResults: CaseResult[] = [];

export function recordCase(result: CaseResult, results: CaseResult[] = caseResults) {
  if (results.some((existing) => existing.name === result.name)) {
    throw new Error(`duplicate differential case name: ${result.name}`);
  }
  results.push(result);
}

export function buildReport(results: CaseResult[], status: 'complete' | 'incomplete', expectedCases = EXPECTED_CASE_COUNT) {
  const names = new Set(results.map((result) => result.name));
  return {
    schema_version: 1,
    status,
    expected_cases: expectedCases,
    observed_cases: results.length,
    missing_cases: Math.max(0, expectedCases - names.size),
    duplicate_cases: results.length - names.size,
    counts: Object.fromEntries((['pass', 'fail', 'error'] as const).map((key) => [
      key, results.filter((result) => result.status === key).length,
    ])),
    cases: [...results].sort((a, b) => a.name.localeCompare(b.name)),
  };
}

export function reconcileCaseCount(observed: number, expected = EXPECTED_CASE_COUNT) {
  if (observed !== expected) {
    throw new Error(`inventory reconciliation failed: expected ${expected}, observed ${observed}`);
  }
}

/**
 * Reconcile names, rather than only counts.  A count-only check can miss a
 * particularly dangerous edit where one case is accidentally dropped while
 * another is added.  Keep this independent of the live runner so the
 * deterministic report tests exercise the same gate as production runs.
 */
export function reconcileCaseInventory(observed: string[], registered: string[]) {
  const registeredSet = new Set(registered);
  const observedSet = new Set(observed);
  const duplicateNames = (names: string[]) => [...new Set(names.filter((name, index) => names.indexOf(name) !== index))].sort();
  const duplicateRegistered = duplicateNames(registered);
  const duplicateObserved = duplicateNames(observed);
  const missing = [...new Set(registered.filter((name) => !observedSet.has(name)))].sort();
  const unexpected = [...new Set(observed.filter((name) => !registeredSet.has(name)))].sort();
  if (duplicateRegistered.length || duplicateObserved.length || missing.length || unexpected.length || observed.length !== registered.length) {
    const details = [
      duplicateRegistered.length ? `duplicate registered: ${duplicateRegistered.join(', ')}` : '',
      duplicateObserved.length ? `duplicate observed: ${duplicateObserved.join(', ')}` : '',
      missing.length ? `missing: ${missing.join(', ')}` : '',
      unexpected.length ? `unexpected: ${unexpected.join(', ')}` : '',
    ].filter(Boolean).join('; ');
    throw new Error(`case inventory reconciliation failed: ${details || `expected ${registered.length}, observed ${observed.length}`}`);
  }
}

async function writeReport(status: 'complete' | 'incomplete', error?: string) {
  const report = buildReport(caseResults, status);
  if (error) report.error = error;
  const destination = Bun.file(REPORT_PATH);
  await Bun.write(destination, JSON.stringify(report, null, 2) + '\n');
}

export function registeredCaseNames(): string[] {
  return registeredCaseGroups().flat().map((testCase) => testCase.name);
}

function registeredCaseCount(): number {
  return registeredCaseGroups().reduce((total, cases) => total + cases.length, 0);
}

function registeredCaseGroups(): Case[][] {
  return [
    CASES,
    KEYS_CASES,
    PROVIDERS_CASES,
    ALIASES_CASES,
    USER_QUOTAS_CASES,
    SYSTEM_SETTINGS_CASES,
    COOLDOWNS_CASES,
    DEBUG_CASES,
    CONFIG_STATUS_CASES,
    TIMEOUT_CONFIG_CASES,
    MCP_ENABLED_CASES,
    EXPLORATION_RATE_CASES,
    FAILOVER_COOLDOWN_CASES,
    CAPTURE_TRACE_CASES,
    STALL_CONFIG_CASES,
    VISION_FALLTHROUGH_CASES,
    LOGGING_CASES,
    BACKGROUND_EXPLORATION_CASES,
    TRUSTED_PROXIES_CASES,
    ERRORS_CASES,
    PERFORMANCE_CASES,
    CONFIG_SNAPSHOT_CASES,
    CONFIG_EXPORT_CASES,
    BACKUP_CASES,
    OBSERVABILITY_CASES,
    CONCURRENCY_CASES,
    VISION_CACHE_CASES,
    OAUTH_PROVIDER_CASES,
    QUOTA_CHECKER_TYPE_CASES,
    PI_PROVIDER_CASES,
    OAUTH_STATUS_CASES,
    OAUTH_DELETE_CASES,
    COMPACTION_CASES,
    QUOTA_DISCOVERY_CASES,
    QUOTA_CLEAR_CASES,
    CUSTOM_CHECKER_CASES,
    MCP_CASES,
    MCP_TOOL_CASES,
    DEBUG_LOG_CASES,
    MCP_LOG_CASES,
    INFERENCE_CASES,
    SELF_CASES,
  ];
}

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

export function normalizeForCase(path: string, body: unknown): unknown {
  const normalized = normalize(body);
  if (path === '/mcp/plexus' && normalized && typeof normalized === 'object') {
    const envelope = structuredClone(normalized) as any;
    const structured = envelope.result?.structuredContent;
    if (structured?.operation === 'refresh_metadata' && structured?.ok === true) {
      const report = structured.data;
      if (!Number.isFinite(Date.parse(report?.refreshedAt)) || !Number.isFinite(report?.durationMs) || report.durationMs < 0) throw new Error('Invalid metadata refresh timing');
      for (const item of envelope.result.content ?? []) {
        if (item.type === 'text' && !deepEqual(JSON.parse(item.text), (normalized as any).result.structuredContent)) throw new Error('MCP text disagrees with structured content');
      }
      report.refreshedAt = '<timestamp>';
      report.durationMs = '<duration-ms>';
      for (const item of envelope.result.content ?? []) if (item.type === 'text') item.text = JSON.stringify(structured);
      return envelope;
    }
    if (structured?.operation === 'backup' && structured?.ok === true && structured.data?.full === false) {
      const backup = structured.data.backup;
      if (!backup || !Number.isFinite(Date.parse(backup.created_at)) || !['sqlite', 'postgres'].includes(backup.dialect)) throw new Error('Invalid MCP backup metadata');
      delete backup.created_at;
      delete backup.dialect;
      // The text block repeats structuredContent; validate it before applying
      // the same metadata normalization rather than dropping that contract.
      for (const item of envelope.result.content ?? []) {
        if (item.type !== 'text') continue;
        const text = JSON.parse(item.text);
        if (!deepEqual(text, (normalized as any).result.structuredContent)) throw new Error('MCP text disagrees with structured content');
        item.text = JSON.stringify(structured);
      }
      return envelope;
    }
  }
  if (path.startsWith('/v0/management/mcp-logs') && normalized && typeof normalized === 'object' && Array.isArray((normalized as any).data)) {
    const page = structuredClone(normalized) as any;
    for (const row of page.data) {
      if (typeof row.request_id !== 'string' || !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(row.request_id)) throw new Error('Invalid MCP audit UUID');
      if (!Number.isFinite(Date.parse(row.created_at)) || !Number.isSafeInteger(row.start_time) || row.start_time <= 0 || !Number.isFinite(row.duration_ms) || row.duration_ms < 0) throw new Error('Invalid MCP audit timing');
      row.request_id = '<uuid>';
      row.created_at = '<timestamp>';
      row.start_time = '<timestamp-ms>';
      row.duration_ms = '<duration-ms>';
    }
    return page;
  }
  if (path.startsWith('/v0/management/backup') && normalized && typeof normalized === 'object' && !Array.isArray(normalized)) {
    const object = { ...(normalized as Record<string, unknown>) };
    delete object.created_at;
    // The reference may run against SQLite while Trefoil uses PostgreSQL;
    // the dialect identifies the storage engine, not the backup contract.
    delete object.dialect;
    return object;
  }
  if (path.includes('/mcp-servers/') && path.endsWith('/stop') && normalized && typeof normalized === 'object') {
    const result = { ...(normalized as Record<string, unknown>) };
    if (typeof result.exitedAt !== 'string' || !Number.isFinite(Date.parse(result.exitedAt))) throw new Error('Invalid stop exitedAt timestamp');
    result.exitedAt = '<timestamp>';
    return result;
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
// Compare decompressed archive entries, never lossy UTF-8 decoded gzip bytes.
// Only metadata already excluded for JSON backups is omitted here.
export function parseBackupArchive(bytes: Uint8Array): unknown {
  const tar = gunzipSync(bytes);
  const result: Record<string, unknown> = {};
  for (let offset = 0; offset + 512 <= tar.length;) {
    const header = tar.subarray(offset, offset + 512);
    if (header.every(value => value === 0)) break;
    const name = header.subarray(0, 100).toString().split('\0')[0];
    const size = parseInt(header.subarray(124, 136).toString().replace(/\0/g, '').trim(), 8);
    if (!Number.isFinite(size) || size < 0 || offset + 512 + size > tar.length || Object.hasOwn(result, name)) throw new Error('Invalid backup tar entry');
    let data = tar.subarray(offset + 512, offset + 512 + size);
    if (name.endsWith('.gz')) data = gunzipSync(data);
    let value: unknown = data.toString('utf8');
    if (name.endsWith('.json')) {
      const parsed = JSON.parse(value as string);
      if (parsed && typeof parsed === 'object') { delete parsed.created_at; delete parsed.dialect; }
      value = parsed;
    }
    result[name] = value;
    offset += 512 + Math.ceil(size / 512) * 512;
  }
  if (!result['manifest.json'] || !result['config.json']) throw new Error('Backup archive missing manifest/config');
  return result;
}

export function parseResponseBody(text: string, contentType: string | null): unknown {
  const trimmed = text.trim();
  const isJson = contentType?.includes('application/json') || contentType?.includes('+json');
  if (isJson || (trimmed.startsWith('{') || trimmed.startsWith('['))) {
    try { return JSON.parse(text); } catch (error) {
      if (isJson) throw new Error(`malformed JSON response: ${String(error)}`);
    }
  }
  return text;
}

export function parseSseResponse(text: string): unknown {
  const data = text.split(/\r?\n/)
    .filter((line) => line.startsWith('data:'))
    .map((line) => line.slice(5).trimStart())
    .join('\n');
  if (!data) throw new Error('MCP SSE response contained no data event');
  return parseResponseBody(data, 'application/json');
}

function deepEqual(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (typeof a !== typeof b) return false;
  if (a === null || b === null) return a === b;
  if (typeof a === 'string' && typeof b === 'string') {
    try {
      const parsedA = JSON.parse(a);
      const parsedB = JSON.parse(b);
      return deepEqual(parsedA, parsedB);
    } catch { return false; }
  }
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
  const responseBody = contentType?.includes('application/gzip')
    ? parseBackupArchive(new Uint8Array(await res.arrayBuffer()))
    : parseResponseBody(await res.text(), contentType);
  return { status: res.status, body: responseBody, contentType };
}

const mcpSessions = new Map<string, string>();

async function fetchMcp(baseUrl: string, body: unknown, adminHeader?: string) {
  const headers: Record<string, string> = {
    Accept: 'application/json, text/event-stream',
    'Content-Type': 'application/json',
  };
  if (adminHeader !== undefined) headers['x-admin-key'] = adminHeader;
  let session = mcpSessions.get(baseUrl);
  if (!mcpSessions.has(baseUrl)) {
    const init = await fetch(`${baseUrl}/mcp/plexus`, {
      method: 'POST', headers,
      body: JSON.stringify({ jsonrpc: '2.0', id: 'parity-init', method: 'initialize', params: { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'trefoil-parity', version: '1' } } }),
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    });
    const initText = await init.text();
    const initBody = init.headers.get('content-type')?.includes('text/event-stream') ? parseSseResponse(initText) : parseResponseBody(initText, init.headers.get('content-type'));
    if (!init.ok || !initBody || typeof initBody !== 'object' || !('result' in initBody)) throw new Error(`MCP initialize failed: ${init.status} ${JSON.stringify(initBody)}`);
    session = init.headers.get('mcp-session-id') ?? '';
    mcpSessions.set(baseUrl, session);
    await fetch(`${baseUrl}/mcp/plexus`, { method: 'POST', headers: { ...headers, 'mcp-session-id': session }, body: JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' }), signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS) });
  }
  if (session) headers['mcp-session-id'] = session;
  const response = await fetch(`${baseUrl}/mcp/plexus`, { method: 'POST', headers, body: JSON.stringify(body), signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS) });
  const contentType = response.headers.get('content-type');
  const raw = await response.text();
  return { status: response.status, body: contentType?.includes('text/event-stream') ? parseSseResponse(raw) : parseResponseBody(raw, contentType), contentType };
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
    name: 'GET key reflects the PATCH',
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
    name: 'KEYS_CASES: GET after delete -> 404',
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
    name: 'GET alias reflects the PATCH',
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
          name: 'ALIASES_CASES: g',
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
    body: { target_groups: [{ name: 'ALIASES_CASES: g', selector: 'random', targets: [{}] }] },
  },
  {
    name: 'PUT rejects an invalid selector',
    method: 'PUT',
    path: `/v0/management/aliases/no-selector-${runId}`,
    adminHeader: ADMIN_KEY,
    body: {
      target_groups: [
        { name: 'ALIASES_CASES: g', selector: 'not-a-selector', targets: [{ provider: 'p', model: 'm' }] },
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
      target_groups: [{ name: 'ALIASES_CASES: g', selector: 'random', targets: [{ provider: 'p', model: 'm' }] }],
    },
  },
  {
    name: 'PUT alias-a -> alias-b succeeds',
    method: 'PUT',
    path: `/v0/management/aliases/alias-a-${runId}`,
    adminHeader: ADMIN_KEY,
    body: { target_groups: [{ name: 'ALIASES_CASES: g', selector: 'random', targets: [{ alias: `alias-b-${runId}` }] }] },
  },
  {
    name: 'PUT alias-b -> alias-a detects the cycle -> 500',
    method: 'PUT',
    path: `/v0/management/aliases/alias-b-${runId}`,
    adminHeader: ADMIN_KEY,
    body: { target_groups: [{ name: 'ALIASES_CASES: g', selector: 'random', targets: [{ alias: `alias-a-${runId}` }] }] },
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
    name: 'ALIASES_CASES: GET after delete -> 404',
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
  { name: 'GET MCP server list returns a collection', method: 'GET', path: '/v0/management/mcp-servers', adminHeader: ADMIN_KEY },
  { name: 'PUT MCP server creates fixture', method: 'PUT', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}`, adminHeader: ADMIN_KEY, body: { upstream_url: 'https://example.invalid/mcp', enabled: true, mode: 'remote_http', auth_scheme: 'bearer' } },
  { name: 'GET MCP server returns persisted configuration', method: 'GET', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}`, adminHeader: ADMIN_KEY },
  { name: 'PATCH MCP server preserves omitted fields', method: 'PATCH', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}`, adminHeader: ADMIN_KEY, body: { enabled: false } },
  { name: 'POST MCP key creates access key', method: 'POST', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}/keys`, adminHeader: ADMIN_KEY, body: { key: `parity-mcp-key-${runId}`, is_active: true } },
  { name: 'GET MCP keys lists persisted access key', method: 'GET', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}/keys`, adminHeader: ADMIN_KEY },
  { name: 'DELETE MCP missing key returns 404', method: 'DELETE', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}/keys/999999`, adminHeader: ADMIN_KEY },
  { name: 'POST MCP missing key clear cooldown returns 404', method: 'POST', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}/keys/999999/clear-cooldown`, adminHeader: ADMIN_KEY },
  { name: 'PUT MCP server rejects invalid mode', method: 'PUT', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}-bad`, adminHeader: ADMIN_KEY, body: { upstream_url: 'https://example.invalid/mcp', mode: 'invalid' } },
  { name: 'GET missing MCP server returns 404', method: 'GET', path: '/v0/management/mcp-servers/parity-missing-server', adminHeader: ADMIN_KEY },
  { name: 'GET MCP server rejects limited key', method: 'GET', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}`, adminHeader: LIMITED_KEY_SECRET },
  { name: 'GET MCP server status returns lifecycle state', method: 'GET', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}/status`, adminHeader: ADMIN_KEY },
  { name: 'GET MCP server process logs returns a page', method: 'GET', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}/process-logs`, adminHeader: ADMIN_KEY },
  { name: 'POST MCP server start validates remote mode', method: 'POST', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}/start`, adminHeader: ADMIN_KEY },
  { name: 'POST MCP server stop validates remote mode', method: 'POST', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}/stop`, adminHeader: ADMIN_KEY },
  { name: 'POST MCP server restart validates remote mode', method: 'POST', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}/restart`, adminHeader: ADMIN_KEY },
  { name: 'DELETE MCP server removes fixture', method: 'DELETE', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}`, adminHeader: ADMIN_KEY },
  { name: 'GET deleted MCP server returns 404', method: 'GET', path: `/v0/management/mcp-servers/${MCP_SERVER_NAME}`, adminHeader: ADMIN_KEY },
];

const MCP_TOOL_OPERATIONS: Array<[string, string]> = [
  ['plexus_config','status'],['plexus_config','get'],['plexus_config','timeout'],['plexus_config','mcp_enabled'],['plexus_config','compaction'],['plexus_config','exploration_rate'],['plexus_config','failover'],['plexus_config','cooldown'],['plexus_config','capture_trace_on_error'],['plexus_config','stall'],['plexus_config','vision_fallthrough'],['plexus_config','background_exploration'],['plexus_config','trusted_proxies'],['plexus_config','export'],
  ['plexus_provider','list'],['plexus_provider','get'],['plexus_provider','put'],['plexus_provider','create'],['plexus_provider','update'],['plexus_provider','delete'],['plexus_provider','fetch_models'],
  ['plexus_model_alias','list'],['plexus_model_alias','get'],['plexus_model_alias','put'],['plexus_model_alias','create'],['plexus_model_alias','update'],['plexus_model_alias','delete'],['plexus_model_alias','delete_all'],
  ['plexus_key','list'],['plexus_key','get'],['plexus_key','put'],['plexus_key','create'],['plexus_key','update'],['plexus_key','delete'],['plexus_key','disable'],
  ['plexus_usage','list'],['plexus_usage','summary'],['plexus_usage','delete'],['plexus_usage','delete_all'],
  ['plexus_mcp_gateway','servers_list'],['plexus_mcp_gateway','list'],['plexus_mcp_gateway','get'],['plexus_mcp_gateway','status'],['plexus_mcp_gateway','process_logs'],['plexus_mcp_gateway','start'],['plexus_mcp_gateway','stop'],['plexus_mcp_gateway','restart'],['plexus_mcp_gateway','put'],['plexus_mcp_gateway','create'],['plexus_mcp_gateway','update'],['plexus_mcp_gateway','delete'],
  ['plexus_settings','get'],['plexus_system_logs','recent'],['plexus_system_logs','level'],['plexus_system_logs','set_level'],['plexus_system_logs','reset_level'],['plexus_system_logs','modules'],['plexus_system_logs','set_modules'],['plexus_system_logs','reset_modules'],
  ['plexus_debug','state'],['plexus_debug','update'],['plexus_debug','logs'],['plexus_debug','get_log'],['plexus_debug','delete_log'],['plexus_debug','delete_all_logs'],
  ['plexus_quota','list'],['plexus_quota','get'],['plexus_quota','put'],['plexus_quota','update'],['plexus_quota','delete'],['plexus_quota','status'],['plexus_quota','clear'],['plexus_quota','recompute'],
  ['plexus_quota_checker','types'],['plexus_quota_checker','list'],['plexus_quota_checker','get'],['plexus_quota_checker','put'],['plexus_quota_checker','update'],['plexus_quota_checker','delete'],
  ['plexus_operations','backup'],['plexus_operations','restore'],['plexus_operations','restart'],['plexus_operations','list_cooldowns'],['plexus_operations','clear_cooldowns'],['plexus_operations','reset_logs'],['plexus_operations','refresh_metadata'],
];
const MCP_TOOL_CASES: KeysCase[] = MCP_TOOL_OPERATIONS.map(([tool, operation]) => ({
  name: `MCP tools/call ${tool}/${operation}`, method: 'POST', path: '/mcp/plexus', adminHeader: ADMIN_KEY,
  body: { jsonrpc: '2.0', id: `${tool}-${operation}`, method: 'tools/call', params: { name: tool, arguments: { operation } } },
}));

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
    let plexus, trefoil;
    try {
      const fetcher = testCase.path === '/mcp/plexus'
        ? (base: string) => fetchMcp(base, testCase.body, testCase.adminHeader)
        : (base: string) => fetchJson(base, testCase.method, testCase.path, testCase.adminHeader, testCase.body, testCase.headers, testCase.rawBody);
      [plexus, trefoil] = await Promise.all([
        fetcher(PLEXUS_BASE_URL),
        fetcher(TREFOIL_BASE_URL),
      ]);
    } catch (error) {
      recordCase({ name: testCase.name, status: 'error', error: String(error) });
      failures += 1;
      console.log(`ERROR ${testCase.name}: ${String(error)}`);
      continue;
    }

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
    const differences = [
      !statusMatch ? `status (plexus ${plexus.status}, trefoil ${trefoil.status})` : '',
      !contentTypeMatch ? `content-type (plexus ${plexus.contentType ?? '<missing>'}, trefoil ${trefoil.contentType ?? '<missing>'})` : '',
      !bodyMatch ? 'response body' : '',
    ].filter(Boolean);
    if (!pass) failures += 1;
    recordCase({ name: testCase.name, status: pass ? 'pass' : 'fail', plexus_status: plexus.status, trefoil_status: trefoil.status,
      ...(differences.length ? { error: `mismatch: ${differences.join(', ')}` } : {}) });

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
  try {
    const total = registeredCaseCount();
    if (total !== EXPECTED_CASE_COUNT) {
      throw new Error(`registered differential inventory drifted: expected ${EXPECTED_CASE_COUNT}, observed ${total}`);
    }
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
    let plexus, trefoil;
    try {
      [plexus, trefoil] = await Promise.all([
        fetchVerify(PLEXUS_BASE_URL, testCase.header),
        fetchVerify(TREFOIL_BASE_URL, testCase.header),
      ]);
    } catch (error) {
      recordCase({ name: testCase.name, status: 'error', error: String(error) });
      failures += 1;
      console.log(`ERROR ${testCase.name}: ${String(error)}`);
      continue;
    }

    const statusMatch = plexus.status === trefoil.status;
    const bodyMatch = deepEqual(normalize(plexus.body), normalize(trefoil.body));
    const pass = statusMatch && bodyMatch;
    const differences = [
      !statusMatch ? `status (plexus ${plexus.status}, trefoil ${trefoil.status})` : '',
      !bodyMatch ? 'response body' : '',
    ].filter(Boolean);
    if (!pass) failures += 1;
    recordCase({ name: testCase.name, status: pass ? 'pass' : 'fail', plexus_status: plexus.status, trefoil_status: trefoil.status,
      ...(differences.length ? { error: `mismatch: ${differences.join(', ')}` } : {}) });

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
  failures += await runCases(BACKUP_CASES.filter(testCase => testCase.method !== 'POST'));
  // A malformed restore may partially replace reference configuration before
  // rejecting. Re-provision shared auth explicitly so later cases test their
  // own contract rather than depending on a previous destructive probe.
  for (const base of [PLEXUS_BASE_URL, TREFOIL_BASE_URL]) {
    await ensureFixture(base);
    for (let attempt = 0; attempt < 40; attempt++) {
      const result = await fetchVerify(base, LIMITED_KEY_SECRET);
      if (result.status === 200) break;
      if (attempt === 39) throw new Error(`Fixture not restored at ${base}`);
      await new Promise(resolve => setTimeout(resolve, 25));
    }
  }

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
  failures += await runCases(MCP_TOOL_CASES);
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
    if (attempt === 19) throw new Error(`Self-service fixture did not become visible at ${PLEXUS_BASE_URL}`);
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  failures += await runCases(selfRestCases);

  // Restore validation can mutate reference settings even when rejecting.
  // Execute these probes after all state-dependent cases, never copy that bug.
  failures += await runCases(BACKUP_CASES.filter(testCase => testCase.method === 'POST'));
  const registeredNames = registeredCaseNames();
  reconcileCaseInventory(caseResults.map((result) => result.name), registeredNames);
  reconcileCaseCount(caseResults.length);
  if (failures > 0) {
    console.error(`\n${failures}/${total} case(s) diverged.`);
    await writeReport('complete');
    process.exit(1);
  }
  await writeReport('complete');
  console.log(`\nAll ${total} cases match.`);
  } catch (error) {
    await writeReport('incomplete', String(error));
    console.error(`\nDifferential run incomplete: ${String(error)}`);
    process.exit(2);
  }
}

if (import.meta.main) main();
