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

export async function getUsageSummary(): Promise<Record<string, number>> {
  const response = await fetch('/v0/management/usage/summary', { headers: authHeaders() });
  if (!response.ok) throw new Error('Unable to load usage summary');
  return response.json();
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
