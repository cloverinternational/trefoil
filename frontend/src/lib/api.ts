import type { Principal } from './types';

const ROLE_ADMIN = 'admin' as const;
const ROLE_LIMITED = 'limited' as const;

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
