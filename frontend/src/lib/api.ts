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
  return response.json();
}

export async function getMetrics(): Promise<string> {
  const response = await fetch('/v0/management/metrics', { headers: authHeaders() });
  if (!response.ok) throw new Error(response.status === 403 ? 'Administrator access required' : 'Unable to load metrics');
  return response.text();
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
  tokensInput: number | null;
  tokensOutput: number | null;
  costTotal: number | null;
}

export async function getRecentUsage(limit = 8): Promise<UsageRecord[]> {
  const response = await fetch(`/v0/management/usage?limit=${limit}&offset=0`, {
    headers: authHeaders(),
  });
  if (!response.ok) throw new Error('Unable to load recent activity');
  const body = (await response.json()) as { data?: UsageRecord[] };
  return Array.isArray(body.data) ? body.data : [];
}

export async function getUsage(limit = 25, offset = 0): Promise<{ data: UsageRecord[]; total: number }> {
  const response = await fetch(`/v0/management/usage?limit=${limit}&offset=${offset}`, { headers: authHeaders() });
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
  display_name?: string;
  api_base_url: string;
  api_key?: string;
  oauth_provider?: string;
  oauth_account?: string;
  enabled: boolean;
  disable_cooldown: boolean;
  stall_cooldown: boolean;
  discount?: number;
  adapter?: { name: string; enabled: boolean }[];
}

export async function getProviders(): Promise<Record<string, ProviderConfig>> {
  const response = await fetch('/v0/management/providers', { headers: authHeaders() });
  if (!response.ok) throw new Error('Unable to load providers');
  return response.json();
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

export async function deleteProvider(name: string): Promise<void> {
  const response = await fetch(`/v0/management/providers/${encodeURIComponent(name)}`, {
    method: 'DELETE', headers: authHeaders(),
  });
  if (!response.ok) throw new Error('Unable to delete provider');
}

export interface OAuthProviderInfo { id: string; name: string; usesCallbackServer?: boolean; }
export interface OAuthCredentialStatus { ready: boolean; }
export interface OAuthSession { id: string; providerId: string; accountId: string; status: string; authInfo?: { url?: string; instructions?: string }; prompt?: { message: string; placeholder?: string; allowEmpty?: boolean }; progress: Array<Record<string, unknown>>; error?: string; }
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

export interface ModelAlias {
  priority?: string;
  target_groups?: ModelTargetGroup[];
  additional_aliases?: string[];
  use_image_fallthrough?: boolean;
  enforce_limits?: boolean;
  sticky_session?: boolean;
  type?: string;
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
