import { expect, test } from '@playwright/test';
import { createServer } from 'node:http';
import { randomUUID } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

test.skip(!process.env.TREFOIL_E2E_BASE_URL, 'Requires local Trefoil Compose');
const admin = readFileSync(resolve(import.meta.dirname, '../../.env'), 'utf8').match(/^ADMIN_KEY=(.*)$/m)?.[1]?.trim();

test('speech bytes, broken upstream and client abort have truthful usage without token throughput', async ({ page, request }) => {
  if (!admin) throw new Error('Local admin credential required');
  const provider = `media-telemetry-e2e-${randomUUID().replaceAll('-', '').slice(0, 8)}`;
  const headers = { 'x-admin-key': admin };
  const server = createServer((req, res) => {
    let body = '';
    req.on('data', chunk => { body += chunk; });
    req.on('end', () => {
      if (req.url !== '/audio/speech') { res.writeHead(404).end(); return; }
      const mode = (JSON.parse(body) as { model?: string }).model;
      res.writeHead(200, { 'content-type': 'audio/mpeg' });
      res.write(Buffer.from([1, 2, 3, 4]));
      if (mode === 'sample') setTimeout(() => res.end(Buffer.from([5, 6])), 60);
      else if (mode === 'broken') setTimeout(() => res.socket?.destroy(), 60);
      else setTimeout(() => { if (!res.destroyed) res.end(Buffer.from([5, 6])); }, 3000);
    });
  });
  await new Promise<void>(done => server.listen(0, '0.0.0.0', done));
  try {
    const origin = `http://192.168.96.1:${(server.address() as { port: number }).port}`;
    expect((await request.put(`/v0/management/providers/${provider}`, { headers, data: { api_base_url: origin, api_key: 'disposable', enabled: true } })).status()).toBe(200);
    const success = await request.post('/v1/audio/speech', { headers: { 'x-api-key': admin }, data: { model: `${provider}/sample`, input: 'hello' } });
    expect(success.status()).toBe(200);
    expect((await success.body()).length).toBe(6);
    await request.post('/v1/audio/speech', { headers: { 'x-api-key': admin }, data: { model: `${provider}/broken`, input: 'hello' } }).catch(() => undefined);
    await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), admin);
    await page.goto('/logs');
    const aborted = await page.evaluate(async ({ provider, key }) => {
      const controller = new AbortController();
      const response = await fetch('/v1/audio/speech', { method: 'POST', signal: controller.signal, headers: { 'x-api-key': key, 'content-type': 'application/json' }, body: JSON.stringify({ model: `${provider}/aborted`, input: 'hello' }) });
      await response.body!.getReader().read();
      const id = response.headers.get('x-request-id');
      controller.abort();
      return id;
    }, { provider, key: admin });
    expect(aborted).toBeTruthy();
    await expect.poll(async () => {
      const response = await request.get('/v0/management/usage?limit=20&offset=0', { headers });
      const rows = ((await response.json()).data as { provider: string; responseStatus: string }[]).filter(row => row.provider === provider);
      return rows.map(row => row.responseStatus).sort();
    }, { timeout: 5000 }).toEqual(['cancelled', 'error', 'success']);
    const usage = await request.get('/v0/management/usage?limit=20&offset=0', { headers });
    const rows = ((await usage.json()).data as { provider: string; ttftMs: number | null; tokensPerSec: number | null; e2eTokensPerSec: number | null; tokensOutput: number | null; responseStatus: string }[]).filter(row => row.provider === provider);
    for (const row of rows) expect(row).toMatchObject({ ttftMs: null, tokensPerSec: null, e2eTokensPerSec: null, tokensOutput: null });
    const performance = await request.get(`/v0/management/performance?provider=${provider}`, { headers });
    expect((await performance.json())).toHaveLength(0);
  } finally {
    await request.delete(`/v0/management/providers/${provider}`, { headers });
    execFileSync('docker', ['compose', 'exec', '-T', 'postgres', 'psql', '-U', 'trefoil', '-d', 'trefoil', '-v', 'ON_ERROR_STOP=1', '-q', '-c', `DELETE FROM provider_performance WHERE provider='${provider}'; DELETE FROM request_usage WHERE provider='${provider}';`], { cwd: resolve(import.meta.dirname, '../..'), stdio: 'pipe', timeout: 15_000 });
    server.closeAllConnections();
    await new Promise<void>(done => server.close(() => done()));
  }
});
