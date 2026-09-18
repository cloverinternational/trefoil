import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  deleteKey,
  getAliases,
  getConfigStatus,
  getMetrics,
  getProviders,
  getUsage,
  patchSystemSettings,
  saveKey,
  saveProvider,
  verifyAdminKey,
} from './api';

const response = (body: unknown, status = 200, headers?: HeadersInit) =>
  new Response(typeof body === 'string' ? body : JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json', ...headers },
  });

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe('frontend management API wire contracts', () => {
  it('verifies admin and limited principals with the exact auth header', async () => {
    const fetchMock = vi.fn().mockResolvedValueOnce(response({ ok: true, role: 'admin' }));
    vi.stubGlobal('fetch', fetchMock);

    await expect(verifyAdminKey('admin-secret')).resolves.toEqual({ role: 'admin' });
    expect(fetchMock).toHaveBeenCalledWith('/v0/management/auth/verify', {
      method: 'GET',
      headers: { 'x-admin-key': 'admin-secret' },
    });
  });

  it('decodes config, providers, aliases, and usage responses while sending auth', async () => {
    vi.stubGlobal('localStorage', { getItem: vi.fn(() => 'dashboard-key') });
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(response({ providerCount: 1, modelAliasCount: 2, keyCount: 3, quotaCount: 4, mcpServerCount: 5 }))
      .mockResolvedValueOnce(response({ openai: { api_base_url: 'https://api.example.com', enabled: true } }))
      .mockResolvedValueOnce(response({ fast: { target_groups: [] } }))
      .mockResolvedValueOnce(response({ data: [{ requestId: 'req-1' }], total: 1 }));
    vi.stubGlobal('fetch', fetchMock);

    await expect(getConfigStatus()).resolves.toEqual({ providerCount: 1, modelAliasCount: 2, keyCount: 3, quotaCount: 4, mcpServerCount: 5 });
    await expect(getProviders()).resolves.toEqual({ openai: { api_base_url: 'https://api.example.com', enabled: true } });
    await expect(getAliases()).resolves.toEqual({ fast: { target_groups: [] } });
    await expect(getUsage(10, 7)).resolves.toEqual({ data: [{ requestId: 'req-1' }], total: 1 });

    expect(fetchMock.mock.calls.map(([url, init]) => [url, (init as RequestInit).headers])).toEqual([
      ['/v0/management/config/status', { 'x-admin-key': 'dashboard-key' }],
      ['/v0/management/providers', { 'x-admin-key': 'dashboard-key' }],
      ['/v0/management/aliases', { 'x-admin-key': 'dashboard-key' }],
      ['/v0/management/usage?limit=10&offset=7', { 'x-admin-key': 'dashboard-key' }],
    ]);
  });

  it('encodes JSON bodies, verbs, paths, and auth for mutations', async () => {
    vi.stubGlobal('localStorage', { getItem: vi.fn(() => 'admin-key') });
    const fetchMock = vi.fn().mockResolvedValue(response(null));
    vi.stubGlobal('fetch', fetchMock);

    await saveProvider('provider/with slash', { enabled: false });
    await saveKey('key/one', { secret: 'secret', comment: 'test' }, true);
    await patchSystemSettings({ max_concurrent_requests: 12 });
    await deleteKey('key/one');

    expect(fetchMock.mock.calls).toEqual([
      ['/v0/management/providers/provider%2Fwith%20slash', { method: 'PUT', headers: { 'x-admin-key': 'admin-key', 'content-type': 'application/json' }, body: '{"enabled":false}' }],
      ['/v0/management/keys/key%2Fone', { method: 'PATCH', headers: { 'x-admin-key': 'admin-key', 'content-type': 'application/json' }, body: '{"secret":"secret","comment":"test"}' }],
      ['/v0/management/system-settings', { method: 'PATCH', headers: { 'x-admin-key': 'admin-key', 'content-type': 'application/json' }, body: '{"max_concurrent_requests":12}' }],
      ['/v0/management/keys/key%2Fone', { method: 'DELETE', headers: { 'x-admin-key': 'admin-key' } }],
    ]);
  });

  it('decodes text metrics and preserves representative HTTP and transport failures', async () => {
    vi.stubGlobal('localStorage', { getItem: vi.fn(() => 'admin-key') });
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(response('trefoil_requests 1', 200, { 'content-type': 'text/plain' }))
      .mockResolvedValueOnce(response({ error: 'nope' }, 403));
    vi.stubGlobal('fetch', fetchMock);

    await expect(getMetrics()).resolves.toBe('trefoil_requests 1');
    await expect(getMetrics()).rejects.toThrow('Administrator access required');
    vi.stubGlobal('fetch', vi.fn().mockRejectedValue(new Error('offline')));
    await expect(verifyAdminKey('bad')).resolves.toBeNull();
  });
});
