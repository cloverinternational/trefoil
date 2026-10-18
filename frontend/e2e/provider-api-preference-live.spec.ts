import { expect, test } from '@playwright/test';
import { mockHost } from './mock-host';
import { createServer } from 'node:http';
import { randomUUID } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

test.skip(!process.env.TREFOIL_E2E_BASE_URL, 'Requires local Trefoil Compose');
const admin = readFileSync(resolve(import.meta.dirname, '../../.env'), 'utf8').match(/^ADMIN_KEY=(.*)$/m)?.[1]?.trim();

test('api_match picks compatible Messages target and falls back for unmatched Chat', async ({ request }) => {
  if (!admin) throw new Error('Local admin key required');
  const suffix = randomUUID().replaceAll('-', '').slice(0, 10);
  const providers = [`prefer-e2e-${suffix}-chat`, `prefer-e2e-${suffix}-messages`];
  const alias = `prefer-e2e-${suffix}-alias`;
  const headers = { 'x-admin-key': admin };
  const observed: string[] = [];
  const servers = providers.map((name, index) => createServer((req, res) => {
    observed.push(`${index}:${req.url}`);
    req.resume();
    req.on('end', () => { res.writeHead(200, { 'content-type': 'application/json' }); res.end(JSON.stringify({ id: 'test', choices: [{ message: { role: 'assistant', content: 'ok' }, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1 } })); });
  }));
  try {
    for (let i = 0; i < servers.length; i++) {
      await new Promise<void>(done => servers[i].listen(0, '0.0.0.0', done));
      const origin = `http://${mockHost}:${(servers[i].address() as { port: number }).port}`;
      const response = await request.put(`/v0/management/providers/${providers[i]}`, { headers, data: { api_base_url: origin, api_key: 'disposable', models: { sample: { type: 'text', access_via: [i === 0 ? 'chat' : 'messages'] } }, enabled: true } });
      expect(response.status()).toBe(200);
    }
    const saved = await request.put(`/v0/management/aliases/${alias}`, { headers, data: { priority: 'api_match', target_groups: [{ name: 'default', selector: 'in_order', targets: providers.map(provider => ({ provider, model: 'sample', enabled: true })) }] } });
    expect(saved.status()).toBe(200);
    const messages = await request.post('/v1/messages', { headers: { 'x-api-key': admin }, data: { model: alias, max_tokens: 4, messages: [{ role: 'user', content: 'ok' }] } });
    expect(messages.status()).toBe(200);
    expect(observed[0]).toBe('1:/messages');
    // Both targets explicitly advertise a non-embeddings API; fallback retains original order.
    const embeddings = await request.post('/v1/embeddings', { headers: { 'x-api-key': admin }, data: { model: alias, input: 'ok' } });
    expect(embeddings.status()).toBe(200);
    expect(observed[1]).toBe('0:/embeddings');
  } finally {
    await request.delete(`/v0/management/models/${alias}`, { headers });
    for (const provider of providers) await request.delete(`/v0/management/providers/${provider}`, { headers });
    execFileSync('docker', ['compose', 'exec', '-T', 'postgres', 'psql', '-U', 'trefoil', '-d', 'trefoil', '-v', 'ON_ERROR_STOP=1', '-q', '-c', `DELETE FROM provider_performance WHERE provider LIKE 'prefer-e2e-${suffix}-%'; DELETE FROM request_usage WHERE provider LIKE 'prefer-e2e-${suffix}-%';`], { cwd: resolve(import.meta.dirname, '../..'), stdio: 'pipe', timeout: 15_000 });
    await Promise.all(servers.map(server => new Promise<void>(done => server.close(() => done()))));
  }
});
