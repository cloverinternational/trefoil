import { expect, test } from '@playwright/test';
import { createServer, type Server } from 'node:http';
import { readFileSync } from 'node:fs';
import { randomUUID } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { resolve } from 'node:path';

test.skip(!process.env.TREFOIL_E2E_BASE_URL, 'Requires local Trefoil Compose');
const key = readFileSync(resolve(import.meta.dirname, '../../.env'), 'utf8').match(/^ADMIN_KEY=(.*)$/m)?.[1]?.trim();
const id = `routing-e2e-${randomUUID().replaceAll('-', '').slice(0, 10)}`;

test('typed URLs and model ID survive reload and reach their intended upstreams', async ({ page, request }) => {
  if (!key) throw new Error('Local admin credential unavailable');
  const observed: { route: 'chat' | 'messages'; model: string }[] = [];
  const servers: Server[] = [];
  const origins: string[] = [];
  try {
    for (const route of ['chat', 'messages'] as const) {
      const server = createServer((req, res) => {
        let text = '';
        req.on('data', chunk => { text += chunk; });
        req.on('end', () => {
          const body = JSON.parse(text) as { model: string };
          if (req.url === (route === 'chat' ? '/chat/completions' : '/messages')) observed.push({ route, model: body.model });
          res.setHeader('content-type', 'application/json');
          res.end(route === 'chat'
            ? JSON.stringify({ id: 'test', object: 'chat.completion', model: body.model, choices: [{ index: 0, message: { role: 'assistant', content: 'acknowledged' }, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 } })
            : JSON.stringify({ id: 'test', type: 'message', role: 'assistant', model: body.model, content: [{ type: 'text', text: 'acknowledged' }], stop_reason: 'end_turn', usage: { input_tokens: 1, output_tokens: 1 } }));
        });
      });
      await new Promise<void>(done => server.listen(0, '0.0.0.0', done));
      servers.push(server);
      origins.push(`http://192.168.96.1:${(server.address() as { port: number }).port}`);
    }
    const headers = { 'x-admin-key': key };
    await page.addInitScript(credential => localStorage.setItem('trefoil_admin_key', credential), key);
    await page.goto('/providers');
    await page.getByRole('button', { name: 'Add provider', exact: true }).click();
    await page.getByLabel('Provider ID').fill(id);
    await page.getByRole('button', { name: 'Add API URL' }).click();
    await page.getByLabel('chat base URL').fill(origins[0]);
    await page.getByRole('button', { name: 'Add API URL' }).click();
    await page.getByRole('combobox', { name: 'API type' }).nth(1).selectOption('messages');
    await page.getByLabel('messages base URL').fill(origins[1]);
    await page.getByRole('button', { name: 'Add model' }).click();
    await page.getByRole('textbox', { name: 'Model ID' }).fill('display-model');
    await page.getByRole('textbox', { name: 'Upstream ID for display-model' }).fill('actual-model');
    await page.getByLabel(/^API key/).fill(`test-${randomUUID()}`);
    await page.getByRole('button', { name: 'Save provider' }).click();
    await expect(page.getByRole('row', { name: new RegExp(id) })).toBeVisible();
    await expect(page.getByRole('row', { name: new RegExp(id) }).getByRole('cell').nth(2)).toHaveText('1');
    await page.reload();
    await page.getByRole('button', { name: `Edit ${id}` }).click();
    await expect(page.getByLabel('chat base URL')).toHaveValue(origins[0]);
    await expect(page.getByLabel('messages base URL')).toHaveValue(origins[1]);
    await expect(page.getByRole('textbox', { name: 'Model ID' })).toHaveValue('display-model');
    await page.getByRole('dialog').getByRole('button', { name: 'Close' }).last().click();
    const saved = (await (await request.get('/v0/management/providers', { headers })).json())[id];
    expect(saved.api_base_urls).toMatchObject({ chat: origins[0], messages: origins[1] });
    expect(saved.models['display-model'].upstream_id).toBe('actual-model');
    const denied = await request.patch(`/v0/management/providers/${id}`, { data: { enabled: false } });
    expect(denied.status()).toBe(401);
    const invalid = await request.patch(`/v0/management/providers/${id}`, { headers, data: { api_base_urls: { chat: 'file:///tmp/not-an-upstream' } } });
    expect(invalid.status()).toBe(400);
    const unchanged = (await (await request.get('/v0/management/providers', { headers })).json())[id];
    expect(unchanged.api_base_urls).toEqual(saved.api_base_urls);
    const unavailable = await request.post('/v1/chat/completions', { headers: { 'x-api-key': key }, data: { model: `${id}/unlisted`, messages: [{ role: 'user', content: 'acknowledge' }] } });
    expect(unavailable.status()).toBe(404);
    const chat = await request.post('/v1/chat/completions', { headers: { 'x-api-key': key }, data: { model: `${id}/display-model`, messages: [{ role: 'user', content: 'acknowledge' }] } });
    expect(chat.status()).toBe(200);
    const messages = await request.post('/v1/messages', { headers: { 'x-api-key': key }, data: { model: `${id}/display-model`, max_tokens: 8, messages: [{ role: 'user', content: 'acknowledge' }] } });
    expect(messages.status()).toBe(200);
    expect(observed).toEqual([{ route: 'chat', model: 'actual-model' }, { route: 'messages', model: 'actual-model' }]);
  } finally {
    try {
      if (key) await request.delete(`/v0/management/providers/${id}`, { headers: { 'x-admin-key': key } });
    } finally {
      // The provider DELETE does not delete historical usage or performance.
      // Only UUID-generated records from this test run are removed.
      execFileSync('docker', ['compose', 'exec', '-T', 'postgres', 'psql', '-U', 'trefoil', '-d', 'trefoil', '-v', 'ON_ERROR_STOP=1', '-q', '-c', `DELETE FROM provider_performance WHERE provider='${id}'; DELETE FROM request_usage WHERE provider='${id}';`], { cwd: resolve(import.meta.dirname, '../..'), stdio: 'pipe', timeout: 15_000 });
    }
    await Promise.all(servers.map(server => new Promise<void>(done => server.close(() => done()))));
  }
});
