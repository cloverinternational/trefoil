import type { Principal } from './types';

const ROLE_ADMIN = 'admin' as const;
const ROLE_LIMITED = 'limited' as const;

function authHeaders(): HeadersInit {
  return { 'x-admin-key': localStorage.getItem('trefoil_admin_key') ?? '' };
}

/**
 * Verify a credential against the backend. Returns the resolved principal on
 * success, or null on 401/network error.
 *
 * Ported from Plexus packages/frontend/src/lib/api/core.ts:verifyAdminKey.
 * Talks to the same GET /v0/management/auth/verify contract, now served by
 * the Rust backend (backend/src/auth.rs).
 */
export async function verifyAdminKey(key: string): Promise<Principal | null> {
  try {
    const res = await fetch('/v0/management/auth/verify', {
      method: 'GET',
      headers: { 'x-admin-key': key },
    });
    if (res.status !== 200) return null;
    const body = (await res.json()) as {
      ok: boolean;
      role: 'admin' | 'limited';
      keyName?: string;
      allowedProviders?: string[];
      allowedModels?: string[];
      excludedProviders?: string[];
      excludedModels?: string[];
      quotaName?: string | null;
      comment?: string | null;
    };
    if (!body.ok) return null;
    if (body.role === ROLE_ADMIN) return { role: ROLE_ADMIN };
    if (body.role === ROLE_LIMITED && typeof body.keyName === 'string') {
      return {
        role: ROLE_LIMITED,
        keyName: body.keyName,
        allowedProviders: Array.isArray(body.allowedProviders) ? body.allowedProviders : [],
        allowedModels: Array.isArray(body.allowedModels) ? body.allowedModels : [],
        excludedProviders: Array.isArray(body.excludedProviders) ? body.excludedProviders : [],
        excludedModels: Array.isArray(body.excludedModels) ? body.excludedModels : [],
        quotaName: typeof body.quotaName === 'string' ? body.quotaName : null,
        comment: typeof body.comment === 'string' ? body.comment : null,
      };
    }
    return null;
  } catch {
    return null;
  }
}

export async function getConfigStatus(): Promise<{
  providerCount: number;
  modelAliasCount: number;
  keyCount: number;
  quotaCount: number;
  mcpServerCount: number;
}> {
  const response = await fetch('/v0/management/config/status', { headers: authHeaders() });
  if (!response.ok) throw new Error('Unable to load configuration status');
  return response.json();
}

export type SystemSettings = Record<string, unknown>;

export async function getSystemSettings(): Promise<SystemSettings> {
  const response = await fetch('/v0/management/system-settings', { headers: authHeaders() });
  if (!response.ok) throw new Error('Unable to load system settings');
  const body = await response.json();
  if (!body || typeof body !== 'object' || Array.isArray(body)) throw new Error('Invalid system settings response');
  return body as SystemSettings;
}

export async function patchSystemSettings(settings: SystemSettings): Promise<void> {
  const response = await fetch('/v0/management/system-settings', { method: 'PATCH', headers: { ...authHeaders(), 'content-type': 'application/json' }, body: JSON.stringify(settings) });
  if (!response.ok) {
    const body = await response.json().catch(() => null) as { error?: string | { message?: string } } | null;
    throw new Error(typeof body?.error === 'string' ? body.error : body?.error?.message || 'Unable to save system settings');
  }
}

export interface LoggingLevelState { level: string; startupLevel: string; supportedLevels: string[]; ephemeral?: boolean; }

async function loggingRequest(path: string, init?: RequestInit): Promise<LoggingLevelState> {
  const response = await fetch(path, { ...init, headers: { ...authHeaders(), ...(init?.headers ?? {}) } });
  const body = await response.json().catch(() => null) as LoggingLevelState & { error?: string | { message?: string } } | null;
  if (!response.ok) throw new Error(typeof body?.error === 'string' ? body.error : body?.error?.message || 'Unable to update logging settings');
  return body as LoggingLevelState;
}
export const getLoggingLevel = () => loggingRequest('/v0/management/logging/level');
export const setLoggingLevel = (level: string) => loggingRequest('/v0/management/logging/level', { method: 'PUT', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ level }) });
export const resetLoggingLevel = () => loggingRequest('/v0/management/logging/level', { method: 'DELETE' });
export async function getModuleFilter(): Promise<{ modules: string[] }> { const r = await fetch('/v0/management/logging/modules', { headers: authHeaders() }); if (!r.ok) throw new Error('Unable to load module filter'); return r.json(); }
export async function setModuleFilter(modules: string[]): Promise<{ modules: string[] }> { const r = await fetch('/v0/management/logging/modules', { method: 'PUT', headers: { ...authHeaders(), 'content-type': 'application/json' }, body: JSON.stringify({ modules }) }); if (!r.ok) throw new Error('Unable to update module filter'); return r.json(); }
export async function clearModuleFilter(): Promise<{ modules: string[] }> { const r = await fetch('/v0/management/logging/modules', { method: 'DELETE', headers: authHeaders() }); if (!r.ok) throw new Error('Unable to clear module filter'); return r.json(); }

export interface DebugState { enabled: boolean; enabledGlobal: boolean; enabledKeys?: string[]; providers: string[] | null; keys: string[]; aliases: string[]; }
export interface DebugLogSummary { requestId: string; createdAt: number; responseStatus: number | null; }
export interface DebugLog { requestId: string; createdAt: number; rawRequest: string | null; transformedRequest: string | null; rawResponse: string | null; transformedResponse: string | null; rawResponseSnapshot: string | null; transformedResponseSnapshot: string | null; requestHeaders: string | null; responseHeaders: string | null; responseStatus: number | null; }
export async function getDebugState(): Promise<DebugState> { const r = await fetch('/v0/management/debug', { headers: authHeaders() }); if (!r.ok) throw new Error('Unable to load debug settings'); return r.json(); }
export async function patchDebugState(update: Partial<Pick<DebugState, 'enabled' | 'providers' | 'keys' | 'aliases'>>): Promise<DebugState> { const r = await fetch('/v0/management/debug', { method: 'PATCH', headers: { ...authHeaders(), 'content-type': 'application/json' }, body: JSON.stringify(update) }); if (!r.ok) throw new Error('Unable to update debug settings'); return r.json(); }
export async function getDebugLogs(limit = 25, offset = 0): Promise<{ data: DebugLogSummary[]; total: number }> { const r = await fetch(`/v0/management/debug/logs?limit=${limit}&offset=${offset}`, { headers: authHeaders() }); if (!r.ok) throw new Error('Unable to load debug logs'); return r.json(); }
export async function getDebugLog(requestId: string): Promise<DebugLog> { const r = await fetch(`/v0/management/debug/logs/${encodeURIComponent(requestId)}`, { headers: authHeaders() }); if (!r.ok) throw new Error('Unable to load debug log'); return r.json(); }
export async function deleteDebugLog(requestId: string): Promise<void> { const r = await fetch(`/v0/management/debug/logs/${encodeURIComponent(requestId)}`, { method: 'DELETE', headers: authHeaders() }); if (!r.ok) throw new Error('Unable to delete debug log'); }
export async function deleteDebugLogs(): Promise<void> { const r = await fetch('/v0/management/debug/logs', { method: 'DELETE', headers: authHeaders() }); if (!r.ok) throw new Error('Unable to delete debug logs'); }

export async function getUsageSummary(): Promise<Record<string, number>> {
  const response = await fetch('/v0/management/usage/summary', { headers: authHeaders() });
  if (!response.ok) throw new Error('Unable to load usage summary');
  const payload = await response.json();
  if (payload?.stats && typeof payload.stats === 'object') {
    const stats = payload.stats;
    return { ...stats, requests: stats.totalRequests ?? 0, errors: stats.totalErrors ?? 0 };
  }
  return payload;
}

export interface UsageBucket { bucketStartMs: number; requests: number; errors: number; inputTokens: number; outputTokens: number; tokens: number; totalCost: number; ttftSamples?: number; tpsSamples?: number; avgTtftMs?: number; avgTokensPerSec?: number; }
export async function getUsageSeries(range: 'day' | 'week' | 'month'): Promise<UsageBucket[]> {
  const response = await fetch(`/v0/management/usage/summary?range=${range}`, { headers: authHeaders() });
  if (!response.ok) throw new Error('Unable to load usage series');
  const payload = await response.json();
  const series = payload?.series ?? payload?.n;
  return Array.isArray(series) ? series as UsageBucket[] : [];
}

export async function getMetrics(): Promise<string> {
  const response = await fetch('/v0/management/metrics', { headers: authHeaders() });
  if (!response.ok) throw new Error(response.status === 403 ? 'Administrator access required' : 'Unable to load metrics');
  return response.text();
}

export async function getBackup(full = false): Promise<Blob> {
  const response = await fetch(`/v0/management/backup${full ? '?full=true' : ''}`, { headers: authHeaders() });
  if (!response.ok) throw new Error(response.status === 403 ? 'Administrator access required' : 'Unable to download backup');
  return response.blob();
}

export async function restoreBackup(backup: BodyInit, contentType = 'application/json'): Promise<Record<string, unknown>> {
  const response = await fetch('/v0/management/restore', { method: 'POST', headers: { ...authHeaders(), 'content-type': contentType }, body: backup });
  const body = await response.json().catch(() => null) as Record<string, unknown> | null;
  if (!response.ok) throw new Error(typeof body?.error === 'string' ? body.error : 'Unable to restore backup');
  if (!body || Array.isArray(body)) throw new Error('Invalid restore response');
  return body;
}

export interface UsageRecord {
  requestId: string;
  date: string;
  apiKey: string | null;
  provider: string | null;
  incomingModelAlias: string | null;
  responseStatus: string | null;
  startTime: number;
  durationMs: number | null;
  /** Time to first token in milliseconds; null/absent when unavailable or non-streaming. */
  ttftMs?: number | null;
  /** Output throughput in tokens per second; null/absent when unavailable or non-streaming. */
  tokensPerSec?: number | null;
  /** End-to-end throughput in tokens per second when reported by the backend. */
  e2eTokensPerSec?: number | null;
  tokensInput: number | null;
  tokensOutput: number | null;
  costTotal: number | null;
}

export interface ProviderPerformanceRow {
  provider: string;
  model: string;
  avg_ttft_ms: number;
  avg_tokens_per_sec: number;
  avg_e2e_tokens_per_sec: number;
  sample_count: number;
  ttft_sample_count: number;
  tps_sample_count: number;
  e2e_sample_count: number;
}
export async function getProviderPerformance(provider = '', model = ''): Promise<ProviderPerformanceRow[]> {
  const query = new URLSearchParams();
  if (provider) query.set('provider', provider);
  if (model) query.set('model', model);
  const response = await fetch(`/v0/management/performance?${query}`, { headers: authHeaders() });
  if (!response.ok) throw new Error(response.status === 403 ? 'Administrator access required' : 'Unable to load provider performance');
  const data = await response.json();
  if (!Array.isArray(data)) throw new Error('Invalid provider performance response');
  return data as ProviderPerformanceRow[];
}

export interface UsageSummaryFull {
  stats: { totalRequests: number; totalErrors: number; totalTokens: number; totalCost: number; avgDurationMs: number; avgTtftMs: number; avgTokensPerSec: number; ttftSamples: number; tpsSamples: number; successRate: number };
  today: { requests: number; totalCost: number };
  series: UsageBucket[];
}
export async function getUsageSummaryFull(range: 'hour' | 'day' | 'week' | 'month' = 'day'): Promise<UsageSummaryFull> {
  const response = await fetch(`/v0/management/usage/summary?range=${range}`, { headers: authHeaders() });
  if (!response.ok) throw new Error('Unable to load usage summary');
  return response.json();
}

export interface ConcurrencyEntry { provider: string; model: string | null; count: number; timestamp: number; }
export async function getConcurrencyLive(): Promise<ConcurrencyEntry[]> {
  const response = await fetch('/v0/management/concurrency?mode=live&groupBy=provider', { headers: authHeaders() });
  if (!response.ok) throw new Error('Unable to load concurrency');
  const payload = await response.json() as { data?: ConcurrencyEntry[] };
  return Array.isArray(payload.data) ? payload.data : [];
}

export interface CooldownEntry { [key: string]: unknown; }
export async function getCooldowns(): Promise<CooldownEntry[]> {
  const response = await fetch('/v0/management/cooldowns', { headers: authHeaders() });
  if (!response.ok) throw new Error('Unable to load cooldowns');
  const payload = await response.json();
  return Array.isArray(payload) ? payload : [];
}

export async function getRecentUsage(limit = 8): Promise<UsageRecord[]> {
  const response = await fetch(`/v0/management/usage?limit=${limit}&offset=0`, {
    headers: authHeaders(),
  });
  if (!response.ok) throw new Error('Unable to load recent activity');
  const body = (await response.json()) as { data?: UsageRecord[] };
  return Array.isArray(body.data) ? body.data : [];
}

export interface UsageFilters { apiKey?: string; incomingModelAlias?: string; provider?: string; startDate?: string; endDate?: string; }
export async function getUsage(limit = 25, offset = 0, filters: UsageFilters = {}): Promise<{ data: UsageRecord[]; total: number }> {
  const params = new URLSearchParams({ limit: String(limit), offset: String(offset) });
  for (const [key, value] of Object.entries(filters)) if (value) params.set(key, value);
  const response = await fetch(`/v0/management/usage?${params}`, { headers: authHeaders() });
  if (!response.ok) throw new Error('Unable to load usage records');
  const body = (await response.json()) as { data?: UsageRecord[]; total?: number };
  const total = Number(body.total ?? 0);
  return { data: Array.isArray(body.data) ? body.data : [], total: Number.isFinite(total) ? total : 0 };
}

export async function deleteUsage(requestId: string): Promise<void> {
  const response = await fetch(`/v0/management/usage/${encodeURIComponent(requestId)}`, { method: 'DELETE', headers: authHeaders() });
  if (!response.ok) throw new Error('Unable to delete usage record');
}

export interface InferenceError { id: number; requestId: string; date: string; errorMessage: string | null; errorStack: string | null; details: string | null; createdAt: number; apiKey: string | null; }
export async function getErrors(limit = 25, offset = 0): Promise<{ data: InferenceError[]; total: number }> { const r = await fetch(`/v0/management/errors?limit=${limit}&offset=${offset}`, { headers: authHeaders() }); if (!r.ok) throw new Error('Unable to load inference errors'); const b = await r.json() as { data?: InferenceError[]; total?: number }; return { data: Array.isArray(b.data) ? b.data : [], total: Number(b.total ?? 0) }; }
export async function deleteError(id: string): Promise<void> { const r = await fetch(`/v0/management/errors/${encodeURIComponent(id)}`, { method: 'DELETE', headers: authHeaders() }); if (!r.ok) throw new Error('Unable to delete inference error'); }
export async function deleteErrors(): Promise<void> { const r = await fetch('/v0/management/errors', { method: 'DELETE', headers: authHeaders() }); if (!r.ok) throw new Error('Unable to clear inference errors'); }

export interface ProviderConfig {
  etag?: string;
  display_name?: string;
  api_base_url: string | Record<string, string>;
  models?: string[] | Record<string, { type?: string; access_via?: string[]; [key: string]: unknown }>;
  api_key?: string;
  oauth_provider?: string;
  oauth_account?: string;
  enabled: boolean;
  disable_cooldown: boolean;
  stall_cooldown: boolean;
  estimateTokens?: boolean;
  useClaudeMasking?: boolean;
  auto_compat?: boolean;
  quota_checker?: { id?: string; type?: string; enabled?: boolean; intervalMinutes?: number; options?: unknown };
  discount?: number;
  adapter?: { name: string; enabled: boolean }[];
}

export interface QuotaCheckerType { type: string; displayName: string }
export async function getQuotaCheckerTypes(): Promise<QuotaCheckerType[]> {
  const response = await fetch('/v0/management/quota-checker-types', { headers: authHeaders() });
  if (!response.ok) throw new Error('Unable to load quota checker types');
  const body = await response.json() as { types?: QuotaCheckerType[] };
  if (!Array.isArray(body.types)) throw new Error('Invalid quota checker types response');
  return body.types.filter(item => typeof item.type === 'string' && typeof item.displayName === 'string');
}

export async function getProviders(): Promise<Record<string, ProviderConfig>> {
  const response = await fetch('/v0/management/providers', { headers: authHeaders() });
  if (!response.ok) throw new Error('Unable to load providers');
  return response.json();
}

export async function getProvider(name: string): Promise<ProviderConfig> {
  const response = await fetch(`/v0/management/providers/${encodeURIComponent(name)}`, { headers: authHeaders() });
  if (!response.ok) throw new Error('Unable to load provider');
  const etag = response.headers.get('ETag');
  if (!etag) throw new Error('Provider response omitted its revision ETag');
  return { ...await response.json() as ProviderConfig, etag };
}

export async function discoverProviderModels(url: string, apiKey?: string, signal?: AbortSignal): Promise<string[]> {
  const response = await fetch('/v0/management/providers/fetch-models', {
    method: 'POST', headers: { ...authHeaders(), 'content-type': 'application/json' },
    body: JSON.stringify({ url, ...(apiKey ? { apiKey } : {}) }),
    signal,
  });
  if (!response.ok) throw new Error(response.status === 400 ? 'Model discovery URL is not allowed' : 'Unable to fetch provider models');
  const body = await response.json() as { data?: { id?: unknown }[] };
  if (!Array.isArray(body.data)) throw new Error('Invalid provider model response');
  return [...new Set(body.data.map(item => item?.id).filter((id): id is string => typeof id === 'string' && id.length > 0 && id.length <= 200))].slice(0, 200);
}

export async function saveProvider(name: string, provider: Record<string, unknown>): Promise<void> {
  const response = await fetch(`/v0/management/providers/${encodeURIComponent(name)}`, {
    method: 'PUT', headers: { ...authHeaders(), 'content-type': 'application/json' }, body: JSON.stringify(provider),
  });
  if (!response.ok) {
    const body = await response.json().catch(() => null) as { error?: string } | null;
    throw new Error(body?.error || 'Unable to save provider');
  }
}

export interface ProviderPreset {
  id: string;
  name: string;
  description?: string;
  suggestedProviderId?: string;
  suggestedName?: string;
  apiBaseUrl: string | Record<string, string>;
  autoCompat?: boolean;
}

export async function getProviderPresets(): Promise<ProviderPreset[]> {
  const response = await fetch('/v0/management/provider-presets', { headers: authHeaders() });
  if (!response.ok) throw new Error('Unable to load provider presets');
  const body = await response.json() as { data?: ProviderPreset[] };
  return Array.isArray(body.data) ? body.data : [];
}

export async function updateProvider(name: string, provider: Record<string, unknown>, etag?: string): Promise<void> {
  const revision = etag ?? (await getProvider(name)).etag;
  if (!revision) throw new Error('Provider revision is unavailable; reload before saving');
  const response = await fetch(`/v0/management/providers/${encodeURIComponent(name)}`, {
    method: 'PATCH', headers: { ...authHeaders(), 'content-type': 'application/json', 'If-Match': revision }, body: JSON.stringify(provider),
  });
  if (!response.ok) {
    const body = await response.json().catch(() => null) as { error?: string } | null;
    throw new Error(body?.error || 'Unable to update provider');
  }
}

export async function deleteProvider(name: string): Promise<void> {
  const response = await fetch(`/v0/management/providers/${encodeURIComponent(name)}`, {
    method: 'DELETE', headers: authHeaders(),
  });
  if (!response.ok) throw new Error('Unable to delete provider');
}

export interface OAuthProviderInfo { id: string; name: string; usesCallbackServer?: boolean; }
/** Plexus credential status: age fields appear only for a ready credential. */
export interface OAuthCredentialStatus { ready: boolean; connectedAt?: number; refreshedAt?: number; expiresAt?: number; }
/** Plexus `OAuthSessionStatus` (oauth-login-session.ts). */
export type OAuthSessionStatus = 'in_progress' | 'awaiting_auth' | 'awaiting_prompt' | 'awaiting_manual_code' | 'awaiting_select' | 'success' | 'error' | 'cancelled';
export const OAUTH_TERMINAL_STATUSES: readonly OAuthSessionStatus[] = ['success', 'error', 'cancelled'];
export interface OAuthSelectOption { id: string; label: string; description?: string; }
/** Plexus `OAuthSession` (the public, `stripInternal` shape). */
export interface OAuthSession {
  id: string;
  providerId: string;
  accountId: string;
  status: OAuthSessionStatus;
  authInfo?: { url: string; instructions?: string };
  prompt?: { message: string; placeholder?: string; allowEmpty?: boolean };
  select?: { message: string; options: OAuthSelectOption[] };
  manualCode?: { message: string; placeholder?: string };
  progress: string[];
  error?: string;
  createdAt: number;
  updatedAt: number;
}
export async function getOAuthProviders(): Promise<OAuthProviderInfo[]> {
  const response = await fetch('/v0/management/oauth/providers', { headers: authHeaders() });
  if (!response.ok) throw new Error(response.status === 403 ? 'Administrator access required' : 'Unable to load OAuth providers');
  const body = await response.json() as { data?: OAuthProviderInfo[] };
  return Array.isArray(body.data) ? body.data : [];
}
export async function getOAuthCredentialStatus(providerId: string, accountId: string): Promise<OAuthCredentialStatus> {
  const query = new URLSearchParams({ providerId, accountId });
  const response = await fetch(`/v0/management/oauth/credentials/status?${query}`, { headers: authHeaders() });
  if (!response.ok) throw new Error('Unable to check OAuth credential status');
  const body = await response.json() as { data?: unknown };
  return { ready: typeof body.data === 'object' && body.data !== null && 'ready' in body.data && typeof body.data.ready === 'boolean' ? body.data.ready : false };
}
export async function deleteOAuthCredentials(providerId: string, accountId: string): Promise<void> {
  const response = await fetch('/v0/management/oauth/credentials', { method: 'DELETE', headers: { ...authHeaders(), 'content-type': 'application/json' }, body: JSON.stringify({ providerId, accountId }) });
  if (!response.ok) { const body = await response.json().catch(() => null) as { error?: string } | null; throw new Error(body?.error || 'Unable to disconnect OAuth account'); }
}

async function oauthSessionRequest(path: string, init?: RequestInit): Promise<OAuthSession> {
  const response = await fetch(path, { ...init, headers: { ...authHeaders(), ...(init?.headers ?? {}) } });
  const body = await response.json().catch(() => null) as { data?: OAuthSession; error?: string } | null;
  if (!response.ok) {
    const detail = typeof body?.error === 'string' ? body.error : 'Unable to update OAuth login session';
    throw new Error(detail);
  }
  if (!body?.data) throw new Error('Invalid OAuth login session response');
  return body.data;
}
export function startOAuthSession(providerId: string, accountId: string): Promise<OAuthSession> { return oauthSessionRequest('/v0/management/oauth/sessions', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ providerId, accountId }) }); }
export function getOAuthSession(id: string): Promise<OAuthSession> { return oauthSessionRequest(`/v0/management/oauth/sessions/${encodeURIComponent(id)}`); }
export function cancelOAuthSession(id: string): Promise<OAuthSession> { return oauthSessionRequest(`/v0/management/oauth/sessions/${encodeURIComponent(id)}/cancel`, { method: 'POST' }); }
export function submitOAuthPrompt(id: string, value?: string): Promise<OAuthSession> { return oauthSessionRequest(`/v0/management/oauth/sessions/${encodeURIComponent(id)}/prompt`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ value: value ?? '' }) }); }
export function submitOAuthManualCode(id: string, value?: string): Promise<OAuthSession> { return oauthSessionRequest(`/v0/management/oauth/sessions/${encodeURIComponent(id)}/manual-code`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ value: value ?? '' }) }); }
export function submitOAuthSelect(id: string, value: string): Promise<OAuthSession> { return oauthSessionRequest(`/v0/management/oauth/sessions/${encodeURIComponent(id)}/select`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ value }) }); }

export interface McpServerConfig { upstreamUrl: string; enabled: boolean; mode: string; authScheme?: string | null; launcher?: string | null; packageName?: string | null; args?: string | null; env?: string | null; startupTimeoutMs?: number | null; }
export interface McpKey { id: number; key: string; isActive: boolean; }
async function mcpRequest(path: string, init?: RequestInit) { return fetch(path, { ...init, headers: { ...authHeaders(), ...(init?.headers ?? {}) } }); }
export async function getMcpServers(): Promise<Record<string, McpServerConfig>> { const r = await mcpRequest('/v0/management/mcp-servers'); if (!r.ok) throw new Error('Unable to load MCP servers'); return r.json(); }
export async function saveMcpServer(name: string, server: Record<string, unknown>, editing: boolean): Promise<void> { const r = await mcpRequest(`/v0/management/mcp-servers/${encodeURIComponent(name)}`, { method: editing ? 'PATCH' : 'PUT', headers: { 'content-type': 'application/json' }, body: JSON.stringify(server) }); if (!r.ok) { const b = await r.json().catch(() => null) as { error?: string } | null; throw new Error(b?.error || 'Unable to save MCP server'); } }
export async function deleteMcpServer(name: string): Promise<void> { const r = await mcpRequest(`/v0/management/mcp-servers/${encodeURIComponent(name)}`, { method: 'DELETE' }); if (!r.ok) throw new Error('Unable to delete MCP server'); }
export async function getMcpKeys(name: string): Promise<McpKey[]> { const r = await mcpRequest(`/v0/management/mcp-servers/${encodeURIComponent(name)}/keys`); if (!r.ok) throw new Error('Unable to load MCP keys'); const b = await r.json() as { keys?: McpKey[] }; return Array.isArray(b.keys) ? b.keys : []; }
export async function addMcpKey(name: string, key: string): Promise<void> { const r = await mcpRequest(`/v0/management/mcp-servers/${encodeURIComponent(name)}/keys`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ key, is_active: true }) }); if (!r.ok) throw new Error('Unable to add MCP key'); }
export async function deleteMcpKey(name: string, id: number): Promise<void> { const r = await mcpRequest(`/v0/management/mcp-servers/${encodeURIComponent(name)}/keys/${id}`, { method: 'DELETE' }); if (!r.ok) throw new Error('Unable to delete MCP key'); }

export interface ModelTarget {
  provider?: string;
  model?: string;
  alias?: string;
  enabled?: boolean;
}

export interface ModelTargetGroup {
  name: string;
  selector: string;
  targets: ModelTarget[];
}

/** Alias `metadata.overrides` (Plexus `MetadataOverridesSchema`). */
export interface MetadataOverrides {
  name?: string;
  /** Compact display label (1–64 chars after trim); derived from `name` when omitted. */
  short_name?: string;
  description?: string;
  context_length?: number;
  [key: string]: unknown;
}

export interface AliasMetadata {
  source: 'auto' | 'disabled' | 'openrouter' | 'models.dev' | 'catwalk' | 'custom';
  source_path?: string;
  overrides?: MetadataOverrides;
}

/** `GET /v1/models` entry (metadata fields present when resolved). */
export interface ModelListEntry {
  id: string;
  name?: string;
  /** Explicit `metadata.overrides.short_name`, otherwise derived from `name`. */
  short_name?: string;
}

export interface ModelAlias {
  priority?: string;
  target_groups?: ModelTargetGroup[];
  additional_aliases?: string[];
  use_image_fallthrough?: boolean;
  enforce_limits?: boolean;
  sticky_session?: boolean;
  type?: string;
  metadata?: AliasMetadata;
}

export async function getAliases(): Promise<Record<string, ModelAlias>> {
  const response = await fetch('/v0/management/aliases', { headers: authHeaders() });
  if (!response.ok) throw new Error('Unable to load model aliases');
  return response.json();
}

async function aliasRequest(slug: string, method: 'PUT' | 'PATCH', body: ModelAlias): Promise<void> {
  const response = await fetch(`/v0/management/aliases/${encodeURIComponent(slug)}`, {
    method,
    headers: { ...authHeaders(), 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (!response.ok) {
    const body = await response.json().catch(() => null) as { error?: string } | null;
    throw new Error(body?.error || 'Unable to save model alias');
  }
}

export function saveAlias(slug: string, alias: ModelAlias, editing: boolean): Promise<void> {
  return aliasRequest(slug, editing ? 'PATCH' : 'PUT', alias);
}

export async function deleteAlias(slug: string): Promise<void> {
  const response = await fetch(`/v0/management/models/${encodeURIComponent(slug)}`, {
    method: 'DELETE', headers: authHeaders(),
  });
  if (!response.ok) throw new Error('Unable to delete model alias');
}

export interface ApiKeyConfig {
  permissionGroup?: string | null;
  secret?: string;
  comment?: string | null;
  quotas?: string[];
  allowedProviders?: string[];
  allowedModels?: string[];
  excludedProviders?: string[];
  excludedModels?: string[];
  allowRawPassthrough?: boolean;
  allowedIps?: string[];
  expiresAt?: number;
  disabledAt?: number;
}

export async function getKeys(): Promise<Record<string, ApiKeyConfig>> {
  const response = await fetch('/v0/management/keys', { headers: authHeaders() });
  if (!response.ok) throw new Error('Unable to load API keys');
  return response.json();
}

export async function saveKey(name: string, key: Record<string, unknown>, editing: boolean): Promise<void> {
  const response = await fetch(`/v0/management/keys/${encodeURIComponent(name)}`, {
    method: editing ? 'PATCH' : 'PUT',
    headers: { ...authHeaders(), 'content-type': 'application/json' },
    body: JSON.stringify(key),
  });
  if (!response.ok) {
    const body = await response.json().catch(() => null) as { error?: string } | null;
    throw new Error(body?.error || 'Unable to save API key');
  }
}

export async function deleteKey(name: string): Promise<void> {
  const response = await fetch(`/v0/management/keys/${encodeURIComponent(name)}`, { method: 'DELETE', headers: authHeaders() });
  if (!response.ok) throw new Error('Unable to delete API key');
}

export async function disableKey(name: string): Promise<void> {
  const response = await fetch(`/v0/management/keys/${encodeURIComponent(name)}/disable`, { method: 'POST', headers: authHeaders() });
  if (!response.ok) {
    const body = await response.json().catch(() => null) as { error?: string } | null;
    throw new Error(body?.error || 'Unable to disable API key');
  }
}

export interface SelfKey { role: 'admin' | 'limited'; keyName?: string; allowedProviders?: string[]; allowedModels?: string[]; excludedProviders?: string[]; excludedModels?: string[]; allowRawPassthrough?: boolean; quotaName?: string | null; comment?: string | null; traceEnabled?: boolean; traceEnabledGlobal?: boolean; }
export interface SelfQuota { key: string; quotaName: string | null; allowed: boolean; currentUsage: number; limit: number | null; remaining: number | null; resetsAt: string | null; limitType: string | null; }
async function selfRequest(path: string, init?: RequestInit) { const response = await fetch(path, { ...init, headers: { ...authHeaders(), ...(init?.headers ?? {}) } }); if (!response.ok) { const body = await response.json().catch(() => null) as { error?: string | { message?: string } } | null; throw new Error(typeof body?.error === 'string' ? body.error : body?.error?.message || 'Unable to update your key'); } return response; }
export async function getSelfKey(): Promise<SelfKey> { return (await selfRequest('/v0/management/self/me')).json(); }
export async function getSelfQuota(): Promise<SelfQuota> { return (await selfRequest('/v0/management/self/quota')).json(); }
export async function updateSelfComment(comment: string): Promise<void> { await selfRequest('/v0/management/self/comment', { method: 'PATCH', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ comment }) }); }
export async function toggleSelfDebug(enabled: boolean): Promise<{ enabled: boolean; enabledGlobal: boolean }> { return (await selfRequest('/v0/management/self/debug/toggle', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ enabled }) })).json(); }
export async function rotateSelfKey(): Promise<{ secret: string; message: string }> { return (await selfRequest('/v0/management/self/rotate', { method: 'POST' })).json(); }

export interface QuotaDefinition {
  type: 'rolling' | 'daily' | 'weekly' | 'monthly';
  limitType: 'requests' | 'tokens' | 'cost';
  limit: number;
  duration?: string;
  warnAt?: number;
  shared?: boolean;
  allowedProviders?: string[];
  excludedProviders?: string[];
  allowedModels?: string[];
  excludedModels?: string[];
}

export interface QuotaSnapshot {
  name: string; limitType: string; limit: number; currentUsage: number;
  remaining: number; allowed: boolean; resetsAt: string; global: boolean;
  shared: boolean; source: string; warnAt?: number;
}

export async function getQuotas(): Promise<Record<string, QuotaDefinition>> {
  const response = await fetch('/v0/management/user-quotas', { headers: authHeaders() });
  if (!response.ok) throw new Error('Unable to load quota definitions');
  return response.json();
}

export async function saveQuota(name: string, quota: QuotaDefinition, editing: boolean): Promise<void> {
  const response = await fetch(`/v0/management/user-quotas/${encodeURIComponent(name)}`, {
    method: editing ? 'PATCH' : 'PUT',
    headers: { ...authHeaders(), 'content-type': 'application/json' }, body: JSON.stringify(quota),
  });
  if (!response.ok) {
    const body = await response.json().catch(() => null) as { error?: { message?: string } } | null;
    throw new Error(body?.error?.message || 'Unable to save quota');
  }
}

export async function deleteQuota(name: string): Promise<void> {
  const response = await fetch(`/v0/management/user-quotas/${encodeURIComponent(name)}`, { method: 'DELETE', headers: authHeaders() });
  if (!response.ok) throw new Error('Unable to delete quota');
}

export async function getQuotaStatus(key: string): Promise<{ key: string; quotas: QuotaSnapshot[] }> {
  const response = await fetch(`/v0/management/quota/status/${encodeURIComponent(key)}`, { headers: authHeaders() });
  if (!response.ok) throw new Error('Unable to load quota status');
  return response.json();
}

export async function resetQuota(key: string, quota?: string): Promise<void> {
  const response = await fetch('/v0/management/quota/clear', { method: 'POST', headers: { ...authHeaders(), 'content-type': 'application/json' }, body: JSON.stringify({ key, ...(quota ? { quota } : {}) }) });
  if (!response.ok) throw new Error('Unable to reset quota');
}

export async function recomputeQuota(key: string, quota: string): Promise<void> {
  const response = await fetch('/v0/management/quota/recompute', { method: 'POST', headers: { ...authHeaders(), 'content-type': 'application/json' }, body: JSON.stringify({ key, quota }) });
  if (!response.ok) throw new Error('Unable to recompute quota');
}

export interface CustomQuotaChecker {
  id: string;
  displayName: string;
  code: string;
  enabled: boolean;
  createdAt?: string;
  updatedAt?: string;
}

export async function getCustomQuotaCheckers(): Promise<CustomQuotaChecker[]> {
  const response = await fetch('/v0/management/custom-checkers', { headers: authHeaders() });
  if (!response.ok) throw new Error('Unable to load custom quota checkers');
  const body = await response.json();
  return Array.isArray(body) ? body : [];
}

export async function saveCustomQuotaChecker(id: string, checker: Pick<CustomQuotaChecker, 'displayName' | 'code' | 'enabled'>): Promise<CustomQuotaChecker> {
  const response = await fetch(`/v0/management/custom-checkers/${encodeURIComponent(id)}`, {
    method: 'PUT', headers: { ...authHeaders(), 'content-type': 'application/json' },
    body: JSON.stringify({ id, ...checker }),
  });
  const body = await response.json().catch(() => null) as CustomQuotaChecker & { error?: string } | null;
  if (!response.ok) throw new Error(body?.error || 'Unable to save custom quota checker');
  return body as CustomQuotaChecker;
}

export async function deleteCustomQuotaChecker(id: string): Promise<void> {
  const response = await fetch(`/v0/management/custom-checkers/${encodeURIComponent(id)}`, { method: 'DELETE', headers: authHeaders() });
  if (!response.ok) throw new Error('Unable to delete custom quota checker');
}

export async function testCustomQuotaChecker(id: string, provider: string, options: Record<string, unknown>, code?: string): Promise<{ success: boolean; meters: unknown[]; error?: string }> {
  const response = await fetch(`/v0/management/custom-checkers/${encodeURIComponent(id)}/test`, {
    method: 'POST', headers: { ...authHeaders(), 'content-type': 'application/json' },
    body: JSON.stringify({ provider, options, ...(code ? { code } : {}) }),
  });
  const body = await response.json().catch(() => null) as { success?: boolean; meters?: unknown[]; error?: string } | null;
  if (!response.ok) throw new Error(body?.error || 'Custom quota checker test failed');
  return { success: body?.success === true, meters: body?.meters ?? [], error: body?.error };
}
