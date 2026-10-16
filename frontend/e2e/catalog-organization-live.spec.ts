import { expect, test } from '@playwright/test';
import { readFileSync } from 'node:fs';
import { randomUUID } from 'node:crypto';
import { resolve } from 'node:path';

test.skip(!process.env.TREFOIL_E2E_BASE_URL, 'Requires local Trefoil Compose');
const adminKey = readFileSync(resolve(import.meta.dirname, '../../.env'), 'utf8').match(/^ADMIN_KEY=(.*)$/m)?.[1]?.trim();
const group = `org-e2e-${randomUUID().replaceAll('-', '').slice(0, 10)}`;
const tag = `tag-${randomUUID().replaceAll('-', '').slice(0, 8)}`;
const pages = [
  { path: '/providers', kind: 'provider' },
  { path: '/models', kind: 'model' },
  { path: '/aliases', kind: 'alias' },
  { path: '/keys', kind: 'key' },
] as const;

test('shared group and per-resource tags persist across all four lists without routing writes', async ({ page, request }) => {
  if (!adminKey) throw new Error('Local admin key required');
  const headers = { 'x-admin-key': adminKey };
  await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), adminKey);
  const touched: { kind: string; id: string }[] = [];
  try {
    await page.goto('/providers');
    await expect(page.getByLabel('New group name')).toBeVisible();
    await page.getByLabel('New group name').fill(group);
    await page.getByRole('button', { name: 'Create group' }).click();
    await expect(page.getByLabel('Filter group').locator('option', { hasText: group })).toHaveCount(1);
    for (const { path, kind } of pages) {
      await page.goto(path);
      await expect(page.getByLabel('Filter group')).toBeVisible();
      const row = page.getByRole('table').getByRole('row').nth(1);
      const select = row.getByRole('combobox', { name: /^Group for / });
      const id = (await select.getAttribute('aria-label'))!.slice('Group for '.length);
      touched.push({ kind, id });
      await select.selectOption(group);
      await expect(select).toHaveValue(group);
      await row.getByRole('button', { name: `Edit tags for ${id}` }).click();
      await row.getByRole('textbox', { name: `Tags for ${id}` }).fill(tag);
      await row.getByRole('button', { name: 'Save' }).click();
      await expect(row.getByText(tag)).toBeVisible();
      await page.getByLabel('Filter group').selectOption({ label: group });
      await page.getByLabel('Filter tag').selectOption(tag);
      await expect(page.getByRole('table').getByRole('row').filter({ hasText: id })).toBeVisible();
      await page.getByLabel(`Search ${kind}s`).fill('not-a-real-catalog-record');
      await expect(page.getByText('No items match these filters.')).toBeVisible();
      await page.getByRole('button', { name: 'Clear filters' }).click();
      await page.reload();
      await expect(page.getByRole('combobox', { name: `Group for ${id}`, exact: true })).toHaveValue(group);
      const response = await request.get('/v0/management/catalog-organization', { headers });
      expect(response.status()).toBe(200);
      const data = await response.json();
      expect(data.items[kind][id]).toEqual({ group, tags: [tag] });
    }
    const invalid = await request.put('/v0/management/catalog-organization/provider/no-such-id', { headers, data: { group: 'unknown-group', tags: [] } });
    expect(invalid.status()).toBe(400);
    const denied = await request.get('/v0/management/catalog-organization');
    expect(denied.status()).toBe(401);
  } finally {
    for (const item of touched) await request.delete(`/v0/management/catalog-organization/${item.kind}/${encodeURIComponent(item.id)}`, { headers });
    await request.delete(`/v0/management/catalog-organization/groups/${group}`, { headers });
  }
});

test('API key listing does not display credential values', async ({ page, request }) => {
  if (!adminKey) throw new Error('Local admin key required');
  await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), adminKey);
  const response = await request.get('/v0/management/keys', { headers: { 'x-admin-key': adminKey } });
  expect(response.status()).toBe(200);
  const values = Object.values(await response.json() as Record<string, { secret?: string }>).map(value => value.secret).filter((value): value is string => !!value);
  await page.goto('/keys');
  await expect(page.getByRole('table')).toBeVisible();
  const text = await page.getByRole('table').innerText();
  for (const value of values) expect(text).not.toContain(value);
});
