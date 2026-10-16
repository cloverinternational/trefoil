import { expect, test } from '@playwright/test';
import { readFileSync } from 'node:fs';
import { randomUUID } from 'node:crypto';
import { resolve } from 'node:path';

test.skip(!process.env.TREFOIL_E2E_BASE_URL, 'Requires local Trefoil Compose');
const admin = readFileSync(resolve(import.meta.dirname, '../../.env'), 'utf8').match(/^ADMIN_KEY=(.*)$/m)?.[1]?.trim();

test('model discovery rejects internal targets and preview cannot mutate provider without save', async ({ page, request }) => {
  if (!admin) throw new Error('Local admin key required');
  const headers = { 'x-admin-key': admin };
  const unusedId = `discovery-unsaved-${randomUUID().replaceAll('-', '').slice(0, 10)}`;
  for (const url of ['http://127.0.0.1:8080/v1/models', 'https://127.0.0.1/v1/models', 'https://localhost/v1/models', 'https://192.168.96.1/v1/models', 'https://169.254.169.254/latest/meta-data/', 'https://example.com:8443/v1/models', 'https://example.com/internal/status', 'https://api.openai.com/v1/models', 'https://openrouter.ai/other/models']) {
    const response = await request.post('/v0/management/providers/fetch-models', { headers, data: { url } });
    expect(response.status()).toBe(400);
  }
  const denied = await request.post('/v0/management/providers/fetch-models', { data: { url: 'https://example.com/v1/models' } });
  expect(denied.status()).toBe(401);
  const credentialed = await request.post('/v0/management/providers/fetch-models', { headers, data: { url: 'https://openrouter.ai/api/v1/models', apiKey: 'disposable-secret-not-forwarded' } });
  expect(credentialed.status()).toBe(400);
  await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), admin);
  await page.goto('/providers');
  await page.getByRole('button', { name: 'Add provider', exact: true }).click();
  await page.getByLabel('Provider ID').fill(unusedId);
  await expect(page.getByRole('button', { name: 'Fetch Models (preview)' })).toBeDisabled();
  await page.getByRole('button', { name: 'Add API URL' }).click();
  await page.getByLabel('chat base URL').fill('http://127.0.0.1:8080/v1');
  await expect(page.getByRole('button', { name: 'Fetch Models (preview)' })).toBeDisabled();
  await page.getByRole('dialog').getByRole('button', { name: 'Close' }).last().click();
  const final = await request.get('/v0/management/providers', { headers });
  expect((await final.json())[unusedId]).toBeUndefined();
});

test('public HTTPS catalog is previewed and selected model stays unsaved until provider save', async ({ page, request }) => {
  if (!admin) throw new Error('Local admin key required');
  const unusedId = `discovery-public-${randomUUID().replaceAll('-', '').slice(0, 10)}`;
  const headers = { 'x-admin-key': admin };
  await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), admin);
  await page.goto('/providers');
  await page.getByRole('button', { name: 'Add provider', exact: true }).click();
  await page.getByLabel('Provider ID').fill(unusedId);
  await page.getByRole('button', { name: 'Add API URL' }).click();
  await page.getByLabel('chat base URL').fill('https://openrouter.ai/api/v1');
  await page.getByRole('button', { name: 'Fetch Models (preview)' }).click();
  const preview = page.getByRole('status').filter({ hasText: 'discovered; no changes saved yet' });
  await expect(preview).toBeVisible({ timeout: 15_000 });
  const firstAdd = page.getByRole('dialog').getByRole('button', { name: 'Add', exact: true }).first();
  await firstAdd.click();
  await expect(page.getByRole('dialog').getByRole('textbox', { name: 'Model ID' })).toHaveCount(1);
  await page.getByRole('dialog').getByRole('button', { name: 'Close' }).last().click();
  const final = await request.get('/v0/management/providers', { headers });
  expect((await final.json())[unusedId]).toBeUndefined();
});

test('failed discovery retains manual draft models and does not persist routing', async ({ page, request }) => {
  if (!admin) throw new Error('Local admin key required');
  const unusedId = `discovery-failed-${randomUUID().replaceAll('-', '').slice(0, 10)}`;
  const headers = { 'x-admin-key': admin };
  await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), admin);
  await page.goto('/providers');
  await page.getByRole('button', { name: 'Add provider', exact: true }).click();
  await page.getByLabel('Provider ID').fill(unusedId);
  await page.getByRole('button', { name: 'Add model' }).click();
  await page.getByRole('textbox', { name: 'Model ID' }).fill('manual-keep');
  await page.getByRole('button', { name: 'Add API URL' }).click();
  await page.getByLabel('chat base URL').fill('http://127.0.0.1:8080/v1');
  await expect(page.getByRole('button', { name: 'Fetch Models (preview)' })).toBeDisabled();
  await expect(page.getByRole('textbox', { name: 'Model ID' })).toHaveValue('manual-keep');
  await page.getByRole('dialog').getByRole('button', { name: 'Close' }).last().click();
  const backend = await request.get('/v0/management/providers', { headers });
  expect((await backend.json())[unusedId]).toBeUndefined();
});

test('discovery response contains only bounded model IDs, not upstream metadata', async ({ request }) => {
  if (!admin) throw new Error('Local admin key required');
  const response = await request.post('/v0/management/providers/fetch-models', { headers: { 'x-admin-key': admin }, data: { url: 'https://openrouter.ai/api/v1/models' } });
  expect(response.status()).toBe(200);
  const body = await response.json() as { data: Record<string, unknown>[] };
  expect(body.data.length).toBeGreaterThan(0);
  expect(body.data.length).toBeLessThanOrEqual(20_000);
  for (const model of body.data) {
    expect(Object.keys(model)).toEqual(['id']);
    expect(typeof model.id).toBe('string');
    expect((model.id as string).length).toBeLessThanOrEqual(200);
  }
});

test('late discovery result cannot populate a different draft or changed URL', async ({ page, request }) => {
  if (!admin) throw new Error('Local admin key required');
  const draft = `discovery-race-${randomUUID().replaceAll('-', '').slice(0, 10)}`;
  const headers = { 'x-admin-key': admin };
  await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), admin);
  await page.route('**/v0/management/providers/fetch-models', async route => {
    await new Promise(done => setTimeout(done, 350));
    await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ data: [{ id: 'stale-discovered-id' }] }) });
  });
  await page.goto('/providers');
  await page.getByRole('button', { name: 'Add provider', exact: true }).click();
  await page.getByRole('button', { name: 'Add API URL' }).click();
  await page.getByLabel('chat base URL').fill('https://openrouter.ai/api/v1');
  await page.getByRole('button', { name: 'Fetch Models (preview)' }).click();
  await page.getByRole('dialog').getByRole('button', { name: 'Close' }).last().click();
  await page.getByRole('button', { name: 'Add provider', exact: true }).click();
  await page.getByLabel('Provider ID').fill(draft);
  await expect(page.getByText('stale-discovered-id')).toHaveCount(0);
  await page.waitForTimeout(500);
  await expect(page.getByText('stale-discovered-id')).toHaveCount(0);
  await page.getByRole('dialog').getByRole('button', { name: 'Close' }).last().click();
  const backend = await request.get('/v0/management/providers', { headers });
  expect((await backend.json())[draft]).toBeUndefined();
});

test('changing the chat URL invalidates an in-flight discovery preview', async ({ page }) => {
  if (!admin) throw new Error('Local admin key required');
  await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), admin);
  await page.route('**/v0/management/providers/fetch-models', async route => {
    await new Promise(done => setTimeout(done, 350));
    await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ data: [{ id: 'old-url-model' }] }) });
  });
  await page.goto('/providers');
  await page.getByRole('button', { name: 'Add provider', exact: true }).click();
  await page.getByRole('button', { name: 'Add API URL' }).click();
  await page.getByLabel('chat base URL').fill('https://openrouter.ai/api/v1');
  await page.getByRole('button', { name: 'Fetch Models (preview)' }).click();
  await page.getByLabel('chat base URL').fill('https://api.openai.com/v1');
  await page.waitForTimeout(500);
  await expect(page.getByText('old-url-model')).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Fetch Models (preview)' })).toBeDisabled();
});

test('closing a draft aborts its pending model-discovery fetch', async ({ page }) => {
  if (!admin) throw new Error('Local admin key required');
  await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), admin);
  let aborted = false;
  page.on('requestfailed', req => { if (req.url().includes('/v0/management/providers/fetch-models')) aborted = true; });
  await page.route('**/v0/management/providers/fetch-models', async route => {
    await new Promise(done => setTimeout(done, 1400));
    await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ data: [{ id: 'must-not-appear' }] }) }).catch(() => undefined);
  });
  await page.goto('/providers');
  await page.getByRole('button', { name: 'Add provider', exact: true }).click();
  await page.getByRole('button', { name: 'Add API URL' }).click();
  await page.getByLabel('chat base URL').fill('https://openrouter.ai/api/v1');
  await page.getByRole('button', { name: 'Fetch Models (preview)' }).click();
  await page.getByRole('dialog').getByRole('button', { name: 'Close' }).last().click();
  await expect.poll(() => aborted, { timeout: 3000 }).toBe(true);
  await page.getByRole('button', { name: 'Add provider', exact: true }).click();
  await expect(page.getByText('must-not-appear')).toHaveCount(0);
  await expect(page.getByRole('alert')).toHaveCount(0);
});
