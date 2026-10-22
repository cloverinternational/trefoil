import { expect, test } from '@playwright/test';
import { mockHost } from './mock-host';
import { createServer } from 'node:http';
import { randomUUID } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

test.skip(!process.env.TREFOIL_E2E_BASE_URL, 'Requires local Trefoil Compose');
const admin = readFileSync(resolve(import.meta.dirname, '../../.env'), 'utf8').match(/^ADMIN_KEY=(.*)$/m)?.[1]?.trim();

test('limited telemetry lists, summaries and analytics cannot read another key or admin performance', async ({ page, request }) => {
  if (!admin) throw new Error('Local admin key required');
  const tag = randomUUID().replaceAll('-', '').slice(0, 10);
  const provider = `scope-telemetry-e2e-${tag}`;
  const names = [`scope-key-${tag}-a`, `scope-key-${tag}-b`];
  const secrets = [`sk-${randomUUID()}`, `sk-${randomUUID()}`];
  const headers = { 'x-admin-key': admin };
  const server = createServer((req, res) => {
    req.resume();
    req.on('end', () => { res.writeHead(200, { 'content-type': 'application/json' }); res.end(JSON.stringify({ id: `test-${tag}`, choices: [{ message: { role: 'assistant', content: 'ok' }, finish_reason: 'stop' }], usage: { prompt_tokens: 2, completion_tokens: 6 } })); });
  });
  await new Promise<void>(done => server.listen(0, '0.0.0.0', done));
  try {
    const origin = `http://${mockHost}:${(server.address() as { port: number }).port}`;
    expect((await request.put(`/v0/management/providers/${provider}`, { headers, data: { api_base_url: origin, api_key: 'disposable', enabled: true } })).status()).toBe(200);
    for (let i = 0; i < 2; i++) expect((await request.put(`/v0/management/keys/${names[i]}`, { headers, data: { secret: secrets[i] } })).status()).toBe(200);
    for (let i = 0; i < 2; i++) expect((await request.post('/v1/chat/completions', { headers: { 'x-api-key': secrets[i] }, data: { model: `${provider}/sample`, messages: [{ role: 'user', content: 'ok' }] } })).status()).toBe(200);
    const own = await request.get(`/v0/management/usage?limit=20&offset=0&api_key=${names[1]}`, { headers: { 'x-admin-key': secrets[0] } });
    expect(own.status()).toBe(200);
    const rows = (await own.json()).data as { apiKey: string; e2eTokensPerSec: number | null }[];
    expect(rows).toHaveLength(1);
    expect(rows[0].apiKey).toBe(names[0]);
    expect(rows[0].e2eTokensPerSec).toBeGreaterThan(0);
    const summary = await request.get('/v0/management/usage/summary?range=day', { headers: { 'x-admin-key': secrets[0] } });
    expect(summary.status()).toBe(200);
    expect((await summary.json()).stats.totalRequests).toBe(1);
    const end = Date.now();
    const analytics = await request.post('/v0/management/analytics/query', { headers: { 'x-admin-key': secrets[0] }, data: { start: end - 60_000, end, dimension: 'apiKey', metric: 'requests' } });
    expect(analytics.status()).toBe(200);
    expect((await analytics.json()).data.map((entry: { label: string }) => entry.label)).toEqual([names[0]]);
    const forged = await request.post('/v0/management/analytics/query', { headers: { 'x-admin-key': secrets[0] }, data: { start: end - 60_000, end, dimension: 'apiKey', metric: 'avgTps', filters: [{ dimension: 'apiKey', value: names[1] }] } });
    expect(forged.status()).toBe(200);
    expect((await forged.json()).data).toHaveLength(0);
    expect((await request.get(`/v0/management/performance?provider=${provider}`, { headers: { 'x-admin-key': secrets[0] } })).status()).toBe(403);
    let adminPerformanceRequests = 0;
    page.on('request', item => { if (item.url().includes('/v0/management/performance')) adminPerformanceRequests++; });
    await page.addInitScript(secret => localStorage.setItem('trefoil_admin_key', secret), secrets[0]);
    await page.goto('/analytics?tab=performance');
    await expect(page.getByRole('heading', { name: 'Performance' }).last()).toBeVisible();
    await expect(page.getByRole('heading', { name: 'Provider performance samples' })).toHaveCount(0);
    await expect(page.getByRole('button', { name: 'Save dashboard' })).toHaveCount(0);
    await page.getByRole('textbox', { name: 'Provider filter' }).fill(provider);
    await expect(page.getByRole('alert')).toHaveCount(0);
    expect(adminPerformanceRequests).toBe(0);
  } finally {
    for (const name of names) await request.delete(`/v0/management/keys/${name}`, { headers });
    await request.delete(`/v0/management/providers/${provider}`, { headers });
    execFileSync('docker', ['compose', 'exec', '-T', 'postgres', 'psql', '-U', 'trefoil', '-d', 'trefoil', '-v', 'ON_ERROR_STOP=1', '-q', '-c', `DELETE FROM provider_performance WHERE provider='${provider}'; DELETE FROM request_usage WHERE provider='${provider}';`], { cwd: resolve(import.meta.dirname, '../..'), stdio: 'pipe', timeout: 15_000 });
    await new Promise<void>(done => server.close(() => done()));
  }
});
