import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  deleteKey,
  deleteUsage,
  getAliases,
  getConfigStatus,
  getMetrics,
  getOAuthSession,
  getOAuthProviders,
  getOAuthCredentialStatus,
  deleteOAuthCredentials,
  getProviders,
  getRecentUsage,
  getUsageSummary,
  getUsage,
  patchSystemSettings,
  saveKey,
  saveProvider,
  startOAuthSession,
  submitOAuthManualCode,
  submitOAuthPrompt,
  cancelOAuthSession,
  verifyAdminKey,
  getMcpServers,
  saveMcpServer,
  deleteMcpServer,
  getMcpKeys,
  addMcpKey,
  deleteMcpKey,
  getLoggingLevel,
  setLoggingLevel,
  resetLoggingLevel,
  getModuleFilter,
  setModuleFilter,
  clearModuleFilter,
  getDebugState,
  patchDebugState,
  getDebugLogs,
  getDebugLog,
  deleteDebugLog,
  deleteDebugLogs,
  getErrors,
  deleteError,
  deleteErrors,
  getKeys,
  disableKey,
  getSelfKey,
  getSelfQuota,
  updateSelfComment,
  toggleSelfDebug,
  rotateSelfKey,
  getQuotas,
  saveQuota,
  deleteQuota,
  getQuotaStatus,
  resetQuota,
  recomputeQuota,
  getCustomQuotaCheckers,
  saveCustomQuotaChecker,
  deleteCustomQuotaChecker,
  testCustomQuotaChecker,
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

  it('normalizes usage list responses and requests recent and summary data', async () => {
    vi.stubGlobal('localStorage', { getItem: vi.fn(() => 'usage-key') });
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(response({ inputTokens: 12, outputTokens: 8, totalCost: 0.4 }))
      .mockResolvedValueOnce(response({ data: [{ requestId: 'recent-1' }] }))
      .mockResolvedValueOnce(response({ data: 'not-an-array', total: 'bad' }));
    vi.stubGlobal('fetch', fetchMock);

    await expect(getUsageSummary()).resolves.toEqual({ inputTokens: 12, outputTokens: 8, totalCost: 0.4 });
    await expect(getRecentUsage(3)).resolves.toEqual([{ requestId: 'recent-1' }]);
    await expect(getUsage()).resolves.toEqual({ data: [], total: 0 });
    expect(fetchMock.mock.calls.map(([url]) => url)).toEqual([
      '/v0/management/usage/summary',
      '/v0/management/usage?limit=3&offset=0',
      '/v0/management/usage?limit=25&offset=0',
    ]);
  });

  it('encodes usage deletion paths and reports usage failures', async () => {
    vi.stubGlobal('localStorage', { getItem: vi.fn(() => 'usage-key') });
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(response(null))
      .mockResolvedValueOnce(response({}, 500));
    vi.stubGlobal('fetch', fetchMock);

    await expect(deleteUsage('request/id')).resolves.toBeUndefined();
    await expect(getUsageSummary()).rejects.toThrow('Unable to load usage summary');
    expect(fetchMock.mock.calls[0]).toEqual(['/v0/management/usage/request%2Fid', { method: 'DELETE', headers: { 'x-admin-key': 'usage-key' } }]);
  });

  it('covers OAuth session lifecycle requests with auth, encoded IDs, and JSON bodies', async () => {
    vi.stubGlobal('localStorage', { getItem: vi.fn(() => 'oauth-admin-key') });
    const session = { id: 'session/1', providerId: 'anthropic', accountId: 'default', status: 'awaiting_code', progress: [] };
    const fetchMock = vi.fn().mockImplementation(() => Promise.resolve(response({ data: session })));
    vi.stubGlobal('fetch', fetchMock);

    await expect(startOAuthSession('anthropic', 'account/1')).resolves.toEqual(session);
    await expect(getOAuthSession('session/1')).resolves.toEqual(session);
    await expect(submitOAuthPrompt('session/1', ' 1234 ')).resolves.toEqual(session);
    await expect(submitOAuthManualCode('session/1', 'https://callback.example/code')).resolves.toEqual(session);
    await expect(cancelOAuthSession('session/1')).resolves.toEqual(session);

    expect(fetchMock.mock.calls).toEqual([
      ['/v0/management/oauth/sessions', { method: 'POST', headers: { 'x-admin-key': 'oauth-admin-key', 'content-type': 'application/json' }, body: '{"providerId":"anthropic","accountId":"account/1"}' }],
      ['/v0/management/oauth/sessions/session%2F1', { headers: { 'x-admin-key': 'oauth-admin-key' } }],
      ['/v0/management/oauth/sessions/session%2F1/prompt', { method: 'POST', headers: { 'x-admin-key': 'oauth-admin-key', 'content-type': 'application/json' }, body: '{"value":" 1234 "}' }],
      ['/v0/management/oauth/sessions/session%2F1/manual-code', { method: 'POST', headers: { 'x-admin-key': 'oauth-admin-key', 'content-type': 'application/json' }, body: '{"value":"https://callback.example/code"}' }],
      ['/v0/management/oauth/sessions/session%2F1/cancel', { method: 'POST', headers: { 'x-admin-key': 'oauth-admin-key' } }],
    ]);
  });

  it('reports unauthenticated, server, and malformed OAuth session responses', async () => {
    vi.stubGlobal('localStorage', { getItem: vi.fn(() => 'oauth-key') });
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(response({ error: 'Administrator access required' }, 401))
      .mockResolvedValueOnce(response({ error: 'session backend unavailable' }, 503))
      .mockResolvedValueOnce(response({}));
    vi.stubGlobal('fetch', fetchMock);

    await expect(getOAuthSession('missing')).rejects.toThrow('Administrator access required');
    await expect(cancelOAuthSession('expired')).rejects.toThrow('session backend unavailable');
    await expect(getOAuthSession('malformed')).rejects.toThrow('Invalid OAuth login session response');
  });

  it('keeps OAuth account management requests token-free and encodes account boundaries', async () => {
    vi.stubGlobal('localStorage', { getItem: vi.fn(() => 'oauth-key') });
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(response({ data: [{ id: 'anthropic', name: 'Anthropic OAuth', usesCallbackServer: false }] }))
      .mockResolvedValueOnce(response({ data: { ready: true } }))
      .mockResolvedValueOnce(response(null));
    vi.stubGlobal('fetch', fetchMock);

    await expect(getOAuthProviders()).resolves.toEqual([
      { id: 'anthropic', name: 'Anthropic OAuth', usesCallbackServer: false },
    ]);
    await expect(getOAuthCredentialStatus('anthropic/provider', 'account one')).resolves.toEqual({ ready: true });
    await expect(deleteOAuthCredentials('anthropic/provider', 'account one')).resolves.toBeUndefined();

    expect(fetchMock.mock.calls).toEqual([
      ['/v0/management/oauth/providers', { headers: { 'x-admin-key': 'oauth-key' } }],
      ['/v0/management/oauth/credentials/status?providerId=anthropic%2Fprovider&accountId=account+one', { headers: { 'x-admin-key': 'oauth-key' } }],
      ['/v0/management/oauth/credentials', { method: 'DELETE', headers: { 'x-admin-key': 'oauth-key', 'content-type': 'application/json' }, body: '{"providerId":"anthropic/provider","accountId":"account one"}' }],
    ]);
    expect(JSON.stringify(fetchMock.mock.calls)).not.toContain('access_token');
    expect(JSON.stringify(fetchMock.mock.calls)).not.toContain('refresh_token');
  });

  it('uses safe defaults for malformed OAuth discovery/status payloads and preserves errors', async () => {
    vi.stubGlobal('localStorage', { getItem: vi.fn(() => 'oauth-key') });
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(response({ data: { not: 'a list' } }))
      .mockResolvedValueOnce(response({ data: {} }))
      .mockResolvedValueOnce(response({ error: 'OAuth credentials not found' }, 404));
    vi.stubGlobal('fetch', fetchMock);

    await expect(getOAuthProviders()).resolves.toEqual([]);
    await expect(getOAuthCredentialStatus('anthropic', 'default')).resolves.toEqual({ ready: false });
    await expect(deleteOAuthCredentials('anthropic', 'default')).rejects.toThrow('OAuth credentials not found');
  });

  it('covers MCP server and key management wire contracts, including encoded names and failures', async () => {
    vi.stubGlobal('localStorage', { getItem: vi.fn(() => 'mcp-admin-key') });
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(response({ 'server/one': { upstreamUrl: 'https://mcp.example', enabled: true } }))
      .mockResolvedValueOnce(response(null))
      .mockResolvedValueOnce(response(null))
      .mockResolvedValueOnce(response({ keys: [{ id: 7, key: 'redacted', isActive: true }] }))
      .mockResolvedValueOnce(response(null))
      .mockResolvedValueOnce(response(null));
    vi.stubGlobal('fetch', fetchMock);

    await expect(getMcpServers()).resolves.toEqual({ 'server/one': { upstreamUrl: 'https://mcp.example', enabled: true } });
    await expect(saveMcpServer('server/one', { enabled: false }, true)).resolves.toBeUndefined();
    await expect(deleteMcpServer('server/one')).resolves.toBeUndefined();
    await expect(getMcpKeys('server/one')).resolves.toEqual([{ id: 7, key: 'redacted', isActive: true }]);
    await expect(addMcpKey('server/one', 'key-value')).resolves.toBeUndefined();
    await expect(deleteMcpKey('server/one', 7)).resolves.toBeUndefined();

    expect(fetchMock.mock.calls).toEqual([
      ['/v0/management/mcp-servers', { headers: { 'x-admin-key': 'mcp-admin-key' } }],
      ['/v0/management/mcp-servers/server%2Fone', { method: 'PATCH', headers: { 'x-admin-key': 'mcp-admin-key', 'content-type': 'application/json' }, body: '{"enabled":false}' }],
      ['/v0/management/mcp-servers/server%2Fone', { method: 'DELETE', headers: { 'x-admin-key': 'mcp-admin-key' } }],
      ['/v0/management/mcp-servers/server%2Fone/keys', { headers: { 'x-admin-key': 'mcp-admin-key' } }],
      ['/v0/management/mcp-servers/server%2Fone/keys', { method: 'POST', headers: { 'x-admin-key': 'mcp-admin-key', 'content-type': 'application/json' }, body: '{"key":"key-value","is_active":true}' }],
      ['/v0/management/mcp-servers/server%2Fone/keys/7', { method: 'DELETE', headers: { 'x-admin-key': 'mcp-admin-key' } }],
    ]);

    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response({ error: 'server unavailable' }, 503)));
    await expect(getMcpServers()).rejects.toThrow('Unable to load MCP servers');
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response({ error: 'invalid launcher' }, 400)));
    await expect(saveMcpServer('server/one', {}, false)).rejects.toThrow('invalid launcher');
  });

  it('covers logging, debug, and inference-error management contracts', async () => {
    vi.stubGlobal('localStorage', { getItem: vi.fn(() => 'ops-key') });
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(response({ level: 'info', startupLevel: 'warn', supportedLevels: ['info', 'warn'] }))
      .mockResolvedValueOnce(response({ level: 'debug', startupLevel: 'warn', supportedLevels: ['debug', 'info', 'warn'] }))
      .mockResolvedValueOnce(response({ level: 'info', startupLevel: 'warn', supportedLevels: ['info', 'warn'] }))
      .mockResolvedValueOnce(response({ modules: ['backend::api'] }))
      .mockResolvedValueOnce(response({ modules: ['backend::api', 'backend::db'] }))
      .mockResolvedValueOnce(response({ modules: [] }))
      .mockResolvedValueOnce(response({ enabled: true, enabledGlobal: false, providers: null, keys: [], aliases: [] }))
      .mockResolvedValueOnce(response({ enabled: false, enabledGlobal: false, providers: ['openai'], keys: ['k'], aliases: [] }))
      .mockResolvedValueOnce(response({ data: [{ requestId: 'req/1' }], total: 1 }))
      .mockResolvedValueOnce(response({ requestId: 'req/1', responseStatus: 500 }))
      .mockResolvedValueOnce(response(null))
      .mockResolvedValueOnce(response(null));
    vi.stubGlobal('fetch', fetchMock);

    await expect(getLoggingLevel()).resolves.toMatchObject({ level: 'info' });
    await expect(setLoggingLevel('debug')).resolves.toMatchObject({ level: 'debug' });
    await expect(resetLoggingLevel()).resolves.toMatchObject({ level: 'info' });
    await expect(getModuleFilter()).resolves.toEqual({ modules: ['backend::api'] });
    await expect(setModuleFilter(['backend::api', 'backend::db'])).resolves.toEqual({ modules: ['backend::api', 'backend::db'] });
    await expect(clearModuleFilter()).resolves.toEqual({ modules: [] });
    await expect(getDebugState()).resolves.toMatchObject({ enabled: true });
    await expect(patchDebugState({ enabled: false, providers: ['openai'], keys: ['k'], aliases: [] })).resolves.toMatchObject({ enabled: false });
    await expect(getDebugLogs(4, 2)).resolves.toEqual({ data: [{ requestId: 'req/1' }], total: 1 });
    await expect(getDebugLog('req/1')).resolves.toMatchObject({ requestId: 'req/1' });
    await expect(deleteDebugLog('req/1')).resolves.toBeUndefined();
    await expect(deleteDebugLogs()).resolves.toBeUndefined();

    expect(fetchMock.mock.calls.slice(0, 6)).toEqual([
      ['/v0/management/logging/level', { headers: { 'x-admin-key': 'ops-key' } }],
      ['/v0/management/logging/level', { method: 'PUT', headers: { 'x-admin-key': 'ops-key', 'content-type': 'application/json' }, body: '{"level":"debug"}' }],
      ['/v0/management/logging/level', { method: 'DELETE', headers: { 'x-admin-key': 'ops-key' } }],
      ['/v0/management/logging/modules', { headers: { 'x-admin-key': 'ops-key' } }],
      ['/v0/management/logging/modules', { method: 'PUT', headers: { 'x-admin-key': 'ops-key', 'content-type': 'application/json' }, body: '{"modules":["backend::api","backend::db"]}' }],
      ['/v0/management/logging/modules', { method: 'DELETE', headers: { 'x-admin-key': 'ops-key' } }],
    ]);
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response({ error: 'Administrator access required' }, 403)));
    await expect(getErrors()).rejects.toThrow('Unable to load inference errors');
  });

  it('covers key, self-service, quota, and custom-checker management requests', async () => {
    vi.stubGlobal('localStorage', { getItem: vi.fn(() => 'management-key') });
    const fetchMock = vi.fn().mockImplementation((url: string) => {
      if (url === '/v0/management/keys') return Promise.resolve(response({ key: { comment: 'ok' } }));
      if (url.includes('/self/me')) return Promise.resolve(response({ role: 'limited', keyName: 'key' }));
      if (url.includes('/self/quota')) return Promise.resolve(response({ key: 'key', allowed: true }));
      if (url === '/v0/management/custom-checkers') return Promise.resolve(response([]));
      if (url.includes('/custom-checkers/') && url.endsWith('/test')) return Promise.resolve(response({ success: true, meters: [] }));
      if (url.includes('/custom-checkers/')) return Promise.resolve(response({ id: 'checker/1', displayName: 'Checker', code: 'return []', enabled: true }));
      if (url === '/v0/management/user-quotas') return Promise.resolve(response({ 'quota/1': { type: 'daily', limitType: 'requests', limit: 10 } }));
      if (url.includes('/quota/status/')) return Promise.resolve(response({ key: 'key', quotas: [] }));
      if (url.includes('/self/debug/toggle')) return Promise.resolve(response({ enabled: true, enabledGlobal: false }));
      if (url.includes('/self/rotate')) return Promise.resolve(response({ secret: 'new-secret', message: 'rotated' }));
      return Promise.resolve(response(null));
    });
    vi.stubGlobal('fetch', fetchMock);

    await expect(getKeys()).resolves.toEqual({ key: { comment: 'ok' } });
    await expect(disableKey('key/1')).resolves.toBeUndefined();
    await expect(getSelfKey()).resolves.toMatchObject({ keyName: 'key' });
    await expect(getSelfQuota()).resolves.toMatchObject({ allowed: true });
    await expect(updateSelfComment('updated')).resolves.toBeUndefined();
    await expect(toggleSelfDebug(true)).resolves.toMatchObject({ enabled: true });
    await expect(rotateSelfKey()).resolves.toMatchObject({ secret: 'new-secret' });
    await expect(getQuotas()).resolves.toHaveProperty('quota/1');
    await expect(saveQuota('quota/1', { type: 'daily', limitType: 'requests', limit: 10 }, true)).resolves.toBeUndefined();
    await expect(deleteQuota('quota/1')).resolves.toBeUndefined();
    await expect(getQuotaStatus('key/1')).resolves.toEqual({ key: 'key', quotas: [] });
    await expect(resetQuota('key/1', 'quota/1')).resolves.toBeUndefined();
    await expect(recomputeQuota('key/1', 'quota/1')).resolves.toBeUndefined();
    await expect(getCustomQuotaCheckers()).resolves.toEqual([]);
    await expect(saveCustomQuotaChecker('checker/1', { displayName: 'Checker', code: 'return []', enabled: true })).resolves.toMatchObject({ id: 'checker/1' });
    await expect(deleteCustomQuotaChecker('checker/1')).resolves.toBeUndefined();
    await expect(testCustomQuotaChecker('checker/1', 'openai', { window: 60 })).resolves.toEqual({ success: true, meters: [] });
    expect(fetchMock.mock.calls.some(([url]) => url === '/v0/management/keys/key%2F1/disable')).toBe(true);
    expect(fetchMock.mock.calls.some(([url]) => url === '/v0/management/quota/clear')).toBe(true);
  });
});
