import { expect, test } from '@playwright/test';
import { readFileSync } from 'node:fs';
import { randomUUID } from 'node:crypto';
import { resolve } from 'node:path';

test.skip(!process.env.TREFOIL_E2E_BASE_URL, 'Requires local Trefoil Compose');
const adminKey = readFileSync(resolve(import.meta.dirname, '../../.env'), 'utf8').match(/^ADMIN_KEY=(.*)$/m)?.[1]?.trim();
const name = `parity-e2e-${randomUUID().replaceAll('-', '').slice(0, 10)}`;

test('provider add, toggle, edit and OAuth entry retain configuration', async ({ page, request }) => {
  if (!adminKey) throw new Error('Local admin key required');
  await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), adminKey);
  try {
    await page.goto('/providers');
    await expect(page.getByRole('heading', { name: 'Providers' }).last()).toBeVisible();
    await expect(page.getByRole('table').locator('th').first()).toContainText('ID / Name');
    await page.getByRole('button', { name: 'Add OAuth account' }).click();
    await expect(page.getByRole('dialog')).toContainText('Account label');
    await expect(page.getByRole('dialog').getByRole('button', { name: 'Continue to login' })).toBeDisabled();
    await page.getByRole('dialog').getByRole('button', { name: 'Close' }).first().click();
    await page.getByRole('button', { name: 'Add provider', exact: true }).click();
    await page.getByLabel('Provider ID').fill(name);
    await page.getByLabel('Display name').fill('E2E disposable provider');
    await page.getByRole('button', { name: 'Add API URL' }).click();
    await page.getByLabel('chat base URL').fill('https://invalid.example.test/v1');
    await page.getByLabel(/^API key/).fill(`test-${randomUUID()}`);
    await page.getByRole('button', { name: 'Save provider' }).click();
    await expect(page.getByRole('row', { name: new RegExp(name) })).toBeVisible();
    let response = await request.get('/v0/management/providers', { headers: { 'x-admin-key': adminKey } });
    expect(response.status()).toBe(200);
    const before = (await response.json())[name];
    expect(before.enabled).toBe(true);
    const row = page.getByRole('row', { name: new RegExp(name) });
    await row.getByRole('switch', { name: `Enable ${name}` }).click();
    await expect(row.getByRole('switch', { name: `Enable ${name}` })).not.toBeChecked();
    response = await request.get('/v0/management/providers', { headers: { 'x-admin-key': adminKey } });
    const after = (await response.json())[name];
    expect(after.enabled).toBe(false);
    expect(after.api_base_url).toBe(before.api_base_url);
    await row.getByRole('button', { name: `Edit ${name}` }).click();
    await expect(page.getByLabel('Provider ID')).toHaveValue(name);
    await expect(page.getByLabel('chat base URL')).toHaveValue(before.api_base_url);
  } finally {
    if (adminKey) await request.delete(`/v0/management/providers/${name}`, { headers: { 'x-admin-key': adminKey } });
  }
});

test('provider preset and edit retain hidden configuration', async ({ page, request }) => {
  if (!adminKey) throw new Error('Local admin key required');
  const headers = { 'x-admin-key': adminKey };
  const name = `preset-e2e-${randomUUID().replaceAll('-', '').slice(0, 10)}`;
  await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), adminKey);
  try {
    await page.goto('/providers');
    await page.getByRole('button', { name: 'Add provider', exact: true }).click();
    await page.getByLabel('Provider preset').selectOption('anthropic');
    await expect(page.getByLabel('messages base URL')).toHaveValue('https://api.anthropic.com/v1');
    await page.getByLabel('Provider ID').fill(name);
    await page.getByLabel(/^API key/).fill(`test-${randomUUID()}`);
    await page.getByRole('button', { name: 'Save provider' }).click();
    const row = page.getByRole('row', { name: new RegExp(name) });
    await expect(row).toBeVisible();
    const patch = await request.patch(`/v0/management/providers/${name}`, { headers, data: { retryPolicy: { max_retries: 2, delayMs: 100, backoffMultiplier: 2 } } });
    expect(patch.status()).toBe(200);
    const before = (await (await request.get('/v0/management/providers', { headers })).json())[name];
    await page.getByRole('button', { name: 'Refresh providers' }).click();
    await expect(page.getByRole('button', { name: 'Refresh providers' })).toBeEnabled();
    await row.getByRole('button', { name: `Edit ${name}` }).click();
    await page.getByLabel('Display name').fill('Updated without resetting configuration');
    await page.getByRole('button', { name: 'Save provider' }).click();
    await expect(page.getByRole('dialog')).toHaveCount(0);
    await page.reload();
    const after = (await (await request.get('/v0/management/providers', { headers })).json())[name];
    expect(after.display_name).toBe('Updated without resetting configuration');
    expect(after.api_base_url).toBe(before.api_base_url);
    expect(after.retryPolicy).toEqual(before.retryPolicy);
    expect(after.api_key).toBe(before.api_key);
  } finally {
    await request.delete(`/v0/management/providers/${name}`, { headers });
  }
});

test('OAuth dialog selects a supported provider and requires an account label without starting consent', async ({ page }) => {
  if (!adminKey) throw new Error('Local admin key required');
  await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), adminKey);
  let sessionPosts = 0;
  page.on('request', req => { if (req.method() === 'POST' && req.url().endsWith('/v0/management/oauth/sessions')) sessionPosts++; });
  await page.goto('/providers');
  await page.getByRole('button', { name: 'Add OAuth account' }).click();
  const dialog = page.getByRole('dialog');
  await expect(dialog.getByLabel('Provider')).toBeVisible();
  await expect(dialog.getByLabel('Account label')).toHaveValue('');
  await expect(dialog.getByRole('button', { name: 'Continue to login' })).toBeDisabled();
  await dialog.getByLabel('Provider').selectOption('openai-codex');
  await dialog.getByLabel('Account label').fill('work-account');
  await expect(dialog.getByRole('button', { name: 'Continue to login' })).toBeEnabled();
  await dialog.getByRole('button', { name: 'Close' }).first().click();
  expect(sessionPosts).toBe(0);
});

test('narrow provider table remains operable without changing live providers', async ({ page }) => {
  if (!adminKey) throw new Error('Local admin key required');
  await page.setViewportSize({ width: 390, height: 844 });
  await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), adminKey);
  await page.goto('/providers');
  await expect(page.getByRole('button', { name: 'Add OAuth account' })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Add provider', exact: true })).toBeVisible();
  await expect(page.getByRole('table')).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
});

test('quota checker registry selection persists and edit preserves stored options', async ({ page, request }) => {
  if (!adminKey) throw new Error('Local admin key required');
  const name = `quota-e2e-${randomUUID().replaceAll('-', '').slice(0, 10)}`;
  const headers = { 'x-admin-key': adminKey };
  await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), adminKey);
  try {
    await page.goto('/providers');
    await page.getByRole('button', { name: 'Add provider', exact: true }).click();
    await page.getByLabel('Provider ID').fill(name);
    await page.getByRole('button', { name: 'Add API URL' }).click();
    await page.getByLabel('chat base URL').fill('https://invalid.example.test/v1');
    await page.getByLabel(/^API key/).fill(`test-${randomUUID()}`);
    await page.getByText('Advanced provider settings').click();
    await page.getByLabel('Quota checker type').selectOption('openrouter');
    await page.getByLabel('Check interval (minutes)').fill('45');
    await page.getByRole('button', { name: 'Save provider' }).click();
    await expect(page.getByRole('row', { name: new RegExp(name) })).toBeVisible();
    const first = (await (await request.get('/v0/management/providers', { headers })).json())[name];
    expect(first.quota_checker).toMatchObject({ type: 'openrouter', intervalMinutes: 45 });
    expect((await request.patch(`/v0/management/providers/${name}`, { headers, data: { quota_checker: { ...first.quota_checker, options: { region: 'test-only' } } } })).status()).toBe(200);
    const patched = (await (await request.get('/v0/management/providers', { headers })).json())[name];
    expect(patched.quota_checker?.options).toEqual({ region: 'test-only' });
    await page.getByRole('button', { name: `Edit ${name}` }).click();
    await page.getByText('Advanced provider settings').click();
    await expect(page.getByLabel('Quota checker type')).toHaveValue('openrouter');
    const concurrent = await request.patch(`/v0/management/providers/${name}`, { headers, data: { quota_checker: { ...patched.quota_checker, options: { region: 'newer' } } } });
    expect(concurrent.status()).toBe(200);
    await page.getByLabel('Display name').fill('Options preserved');
    await page.getByRole('button', { name: 'Save provider' }).click();
    await expect(page.getByRole('alert')).toContainText('Quota checker changed since this editor opened');
    const conflicted = (await (await request.get('/v0/management/providers', { headers })).json())[name];
    expect(conflicted.quota_checker?.options).toEqual({ region: 'newer' });
    await page.getByRole('dialog').getByRole('button', { name: 'Close' }).last().click();
    await page.getByRole('button', { name: 'Refresh providers' }).click();
    await expect(page.getByRole('button', { name: 'Refresh providers' })).toBeEnabled();
    await page.getByRole('button', { name: `Edit ${name}` }).click();
    await page.getByText('Advanced provider settings').click();
    await page.getByLabel('Display name').fill('Options preserved');
    await page.getByRole('button', { name: 'Save provider' }).click();
    await expect(page.getByRole('dialog')).toHaveCount(0);
    const updated = (await (await request.get('/v0/management/providers', { headers })).json())[name];
    expect(updated.quota_checker).toMatchObject({ type: 'openrouter', intervalMinutes: 45, options: { region: 'newer' } });
    await page.getByRole('button', { name: `Edit ${name}` }).click();
    await page.getByText('Advanced provider settings').click();
    await page.getByLabel('Quota checker type').selectOption('');
    await expect(page.getByLabel('Quota checker type')).toHaveValue('');
    const disabling = page.waitForRequest(req => req.url().includes(`/v0/management/providers/${name}`) && req.method() === 'PATCH');
    await page.getByRole('button', { name: 'Save provider' }).click();
    const sentRequest = await disabling;
    const sent = sentRequest.postDataJSON() as { quota_checker?: { enabled?: boolean } };
    expect(sent.quota_checker?.enabled).toBe(false);
    expect((await sentRequest.response())?.status()).toBe(200);
    const disabled = (await (await request.get('/v0/management/providers', { headers })).json())[name];
    expect(disabled.quota_checker).toMatchObject({ type: 'openrouter', enabled: false, options: { region: 'newer' } });
    await page.getByRole('button', { name: `Edit ${name}` }).click();
    await page.getByText('Advanced provider settings').click();
    await page.getByLabel('Quota checker type').selectOption('openrouter');
    const enabling = page.waitForResponse(res => res.url().includes(`/v0/management/providers/${name}`) && res.request().method() === 'PATCH');
    await page.getByRole('button', { name: 'Save provider' }).click();
    expect((await enabling).status()).toBe(200);
    const reenabled = (await (await request.get('/v0/management/providers', { headers })).json())[name];
    expect(reenabled.quota_checker).toMatchObject({ type: 'openrouter', enabled: true, options: { region: 'newer' } });
  } finally {
    const removed = await request.delete(`/v0/management/providers/${name}`, { headers });
    expect(removed.ok()).toBe(true);
  }
});

test('conditional provider edit is atomic and rejects stale full-record version', async ({ request }) => {
  if (!adminKey) throw new Error('Local admin key required');
  const name = `version-e2e-${randomUUID().replaceAll('-', '').slice(0, 10)}`;
  const headers = { 'x-admin-key': adminKey };
  try {
    expect((await request.put(`/v0/management/providers/${name}`, { headers, data: { api_base_url: 'https://invalid.example.test/v1', api_key: 'disposable', display_name: 'Before', enabled: true } })).status()).toBe(200);
    const original = (await (await request.get('/v0/management/providers', { headers })).json())[name];
    expect(original.version).toBeGreaterThan(0);
    const first = await request.patch(`/v0/management/providers/${name}`, { headers, data: { expectedVersion: original.version, display_name: 'First' } });
    expect(first.status()).toBe(200);
    const stale = await request.patch(`/v0/management/providers/${name}`, { headers, data: { expectedVersion: original.version, display_name: 'Stale', quota_checker: { type: 'openrouter', enabled: false, intervalMinutes: 45, options: { region: 'new' } } } });
    expect(stale.status()).toBe(409);
    const after = (await (await request.get('/v0/management/providers', { headers })).json())[name];
    expect(after.display_name).toBe('First');
    expect(after.version).toBe(original.version + 1);
    expect(after.quota_checker).toBeUndefined();
  } finally {
    await request.delete(`/v0/management/providers/${name}`, { headers });
  }
});

test('stale browser editor reports conflict and leaves newer provider routing intact', async ({ page, request }) => {
  if (!adminKey) throw new Error('Local admin key required');
  const name = `editor-version-e2e-${randomUUID().replaceAll('-', '').slice(0, 10)}`;
  const headers = { 'x-admin-key': adminKey };
  await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), adminKey);
  try {
    expect((await request.put(`/v0/management/providers/${name}`, { headers, data: { api_base_url: 'https://invalid.example.test/v1', api_key: 'disposable', display_name: 'Original', enabled: true } })).status()).toBe(200);
    await page.goto('/providers');
    await page.getByRole('button', { name: `Edit ${name}` }).click();
    await page.getByLabel('Display name').fill('Stale editor');
    const concurrent = await request.patch(`/v0/management/providers/${name}`, { headers, data: { api_base_url: 'https://newer.example.test/v1' } });
    expect(concurrent.status()).toBe(200);
    await page.getByRole('button', { name: 'Save provider' }).click();
    await expect(page.getByRole('alert')).toContainText('Provider changed; reload before saving');
    const saved = (await (await request.get('/v0/management/providers', { headers })).json())[name];
    expect(saved.api_base_url).toBe('https://newer.example.test/v1');
    expect(saved.display_name).toBe('Original');
  } finally {
    await request.delete(`/v0/management/providers/${name}`, { headers });
  }
});

test('provider create collision returns conflict without replacing routing or key', async ({ request }) => {
  if (!adminKey) throw new Error('Local admin key required');
  const name = `create-e2e-${randomUUID().replaceAll('-', '').slice(0, 10)}`;
  const headers = { 'x-admin-key': adminKey };
  try {
    expect((await request.put(`/v0/management/providers/${name}`, { headers, data: { api_base_url: 'https://first.example.test/v1', api_key: 'first-disposable', display_name: 'First', enabled: true } })).status()).toBe(200);
    const before = (await (await request.get('/v0/management/providers', { headers })).json())[name];
    expect((await request.put(`/v0/management/providers/${name}`, { headers, data: { api_base_url: 'https://second.example.test/v1', api_key: 'second-disposable', display_name: 'Second', enabled: false } })).status()).toBe(409);
    const after = (await (await request.get('/v0/management/providers', { headers })).json())[name];
    expect(after.api_base_url).toBe(before.api_base_url);
    expect(after.api_key).toBe(before.api_key);
    expect(after.enabled).toBe(true);
    expect(after.version).toBe(before.version);
  } finally {
    await request.delete(`/v0/management/providers/${name}`, { headers });
  }
});
