import { expect, test } from '@playwright/test';
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { randomUUID } from 'node:crypto';
import { resolve } from 'node:path';

// This is a server-backed browser E2E flow, never a mocked API or isolated function test.
// Run only against the local Compose instance with TREFOIL_E2E_BASE_URL set.
test.skip(!process.env.TREFOIL_E2E_BASE_URL, 'Requires the local Compose stack');
test.describe.configure({ mode: 'serial' });

const root = resolve(import.meta.dirname, '../..');
const adminKey = readFileSync(resolve(root, '.env'), 'utf8').match(/^ADMIN_KEY=(.*)$/m)?.[1]?.trim();
const tag = `analytics-e2e-${randomUUID().replace(/-/g, '').slice(0, 12)}`;
const firstKey = `${tag}-a`;
const secondKey = `${tag}-b`;
const firstSecret = `sk-${randomUUID()}`;
const secondSecret = `sk-${randomUUID()}`;
const firstModel = `${tag}-model-a`;
const secondModel = `${tag}-model-b`;
const firstGroup = `${tag}-team-a`;
const secondGroup = `${tag}-team-b`;
let dashboardId: string | null = null;

function sql(statement: string) {
  execFileSync('docker', ['compose', 'exec', '-T', 'postgres', 'psql', '-U', 'trefoil', '-d', 'trefoil', '-v', 'ON_ERROR_STOP=1', '-q', '-c', statement], { cwd: root, stdio: 'pipe', timeout: 15000 });
}

test.beforeAll(async ({ request }) => {
  if (!adminKey) throw new Error('Local ADMIN_KEY is required for live E2E');
  for (const [name, secret, group] of [[firstKey, firstSecret, firstGroup], [secondKey, secondSecret, secondGroup]]) {
    const response = await request.put(`/v0/management/keys/${name}`, { headers: { 'x-admin-key': adminKey }, data: { secret, permissionGroup: group } });
    expect(response.ok()).toBeTruthy();
  }
  // All interpolated identifiers are generated from UUID hex above. The two TTFT/TPS
  // samples are deliberately different; one missing sample checks null handling.
  const now = Date.now();
  sql(`INSERT INTO request_usage (request_id,date,api_key,permission_group,provider,incoming_model_alias,start_time,created_at,response_status,tokens_output,ttft_ms,tokens_per_sec) VALUES
    ('${tag}-1','${tag}','${firstKey}','${firstGroup}','fixture','${firstModel}',${now - 4000},${now - 4000},'success',20,100,40),
    ('${tag}-2','${tag}','${firstKey}','${firstGroup}','fixture','${firstModel}',${now - 3000},${now - 3000},'success',30,300,60),
    ('${tag}-3','${tag}','${firstKey}','${firstGroup}','fixture','${firstModel}',${now - 2000},${now - 2000},'success',NULL,NULL,NULL),
    ('${tag}-4','${tag}','${secondKey}','${secondGroup}','fixture','${secondModel}',${now - 1000},${now - 1000},'success',40,900,90)`);
});

test.afterAll(async ({ request }) => {
  // Only this test's labeled records, keys and dashboard are removed.
  try { sql(`DELETE FROM request_usage WHERE request_id LIKE '${tag}-%'`); } finally {
    if (adminKey) {
      if (dashboardId) await request.delete(`/v0/management/analytics/dashboards/${dashboardId}`, { headers: { 'x-admin-key': adminKey } });
      for (const name of [firstKey, secondKey]) await request.delete(`/v0/management/keys/${name}`, { headers: { 'x-admin-key': adminKey } });
    }
  }
});

test('Usage and Performance panels use server telemetry and limited scope', async ({ page, request }) => {
  if (!adminKey) throw new Error('Local ADMIN_KEY missing');
  await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), adminKey);
  await page.goto('/analytics?tab=usage');
  await expect(page.getByRole('heading', { name: 'Usage Analytics' })).toBeVisible();
  await expect(page.getByLabel('Panel title').first()).toBeVisible();
  await expect(page.getByRole('alert')).toHaveCount(0);
  await page.getByLabel('Permission group filter').fill(firstGroup);
  await expect(page.getByText(firstGroup, { exact: true }).first()).toBeVisible();
  await page.getByLabel('Permission group filter').fill(secondGroup);
  await expect(page.getByText(secondGroup, { exact: true }).first()).toBeVisible();
  await page.getByLabel('Permission group filter').fill('');
  const end = Date.now();
  const query = { start: end - 60_000, end, dimension: 'model', metric: 'avgTps', filters: [{ dimension: 'permissionGroup', value: firstGroup }] };
  const admin = await request.post('/v0/management/analytics/query', { headers: { 'x-admin-key': adminKey }, data: query });
  expect(admin.status()).toBe(200);
  const rows = (await admin.json()).data as { label: string; value: number; samples: number }[];
  expect(rows.find(row => row.label === firstModel)).toMatchObject({ value: 50, samples: 2 });
  await page.getByRole('button', { name: 'Performance' }).click();
  await expect(page.getByRole('heading', { name: 'Performance', exact: true })).toBeVisible();
  await page.getByLabel('Model filter').fill(firstModel);
  await expect(page.getByText('50 tokens/second', { exact: false }).first()).toBeVisible();
  await expect(page.getByText('200 milliseconds', { exact: false }).first()).toBeVisible();
  const limited = await request.post('/v0/management/analytics/query', { headers: { 'x-admin-key': firstSecret }, data: { ...query, filters: [] } });
  expect(limited.status()).toBe(200);
  expect((await limited.json()).data.map((row: { label: string }) => row.label)).not.toContain(secondModel);
  const forged = await request.post('/v0/management/analytics/query', { headers: { 'x-admin-key': firstSecret }, data: { ...query, filters: [{ dimension: 'permissionGroup', value: secondGroup }] } });
  expect(forged.status()).toBe(200);
  expect((await forged.json()).data).toHaveLength(0);
  expect((await request.post('/v0/management/analytics/query', { headers: { 'x-admin-key': adminKey }, data: { ...query, dimension: 'raw_sql' } })).status()).toBe(400);
});

test('admin saves a dashboard; limited viewer reads it but cannot edit', async ({ page, request }) => {
  if (!adminKey) throw new Error('Local ADMIN_KEY missing');
  await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), adminKey);
  await page.goto('/analytics?tab=usage');
  await page.getByLabel('Dashboard name').fill(tag);
  await page.getByRole('button', { name: 'Save dashboard' }).click();
  await expect(page.getByRole('status')).toContainText('Dashboard saved');
  const all = await request.get('/v0/management/analytics/dashboards', { headers: { 'x-admin-key': adminKey } });
  const saved = (await all.json() as { id: string; name: string }[]).find(item => item.name === tag);
  expect(saved?.id).toBeTruthy();
  dashboardId = saved!.id;
  await page.reload();
  await expect(page.getByRole('heading', { name: 'Usage Analytics' })).toBeVisible();
  await page.getByLabel('Saved dashboard').click();
  await page.getByRole('option', { name: tag }).click();
  await expect(page.getByLabel('Dashboard name')).toHaveValue(tag);
  const denied = await request.put(`/v0/management/analytics/dashboards/${dashboardId}`, { headers: { 'x-admin-key': firstSecret }, data: { name: 'denied', config: { panels: [] }, version: 1 } });
  expect(denied.status()).toBe(403);
  const read = await request.get(`/v0/management/analytics/dashboards/${dashboardId}`, { headers: { 'x-admin-key': firstSecret } });
  expect(read.status()).toBe(200);
});

test('metric formula and group snapshot survive key reassignment', async ({ request }) => {
  if (!adminKey) throw new Error('Local ADMIN_KEY missing');
  const changed = await request.patch(`/v0/management/keys/${firstKey}`, { headers: { 'x-admin-key': adminKey }, data: { permissionGroup: secondGroup } });
  expect(changed.status()).toBe(200);
  const end = Date.now();
  const base = { start: end - 60_000, end, dimension: 'permissionGroup', metric: 'requests' };
  const historical = await request.post('/v0/management/analytics/query', { headers: { 'x-admin-key': adminKey }, data: base });
  expect(historical.status()).toBe(200);
  const grouped = (await historical.json()).data as { label: string; value: number }[];
  expect(grouped.find(row => row.label === firstGroup)?.value).toBe(3);
  expect(grouped.find(row => row.label === secondGroup)?.value).toBe(1);
  const formula = await request.post('/v0/management/analytics/query', { headers: { 'x-admin-key': adminKey }, data: { ...base, dimension: 'model', filters: [{ dimension: 'permissionGroup', value: firstGroup }], formula: 'outputTokens / requests' } });
  expect(formula.status()).toBe(200);
  expect((await formula.json()).data.find((row: { label: string }) => row.label === firstModel)?.value).toBeCloseTo(50 / 3, 3);
  const rejected = await request.post('/v0/management/analytics/query', { headers: { 'x-admin-key': adminKey }, data: { ...base, formula: 'drop table request_usage' } });
  expect(rejected.status()).toBe(400);
});

test('limited viewer opens shared dashboard without editor or other key data', async ({ browser, request }) => {
  if (!adminKey || !dashboardId) throw new Error('Admin and saved dashboard are required');
  const created = await request.post('/v0/management/analytics/dashboards', { headers: { 'x-admin-key': adminKey }, data: { name: `${tag}-viewer`, config: { panels: [{ id: 'group-panel', title: 'Requests by group', metric: 'requests', dimension: 'permissionGroup' }], range: 'day', filters: {} } } });
  expect(created.status()).toBe(200);
  const viewerDashboardId = (await created.json()).id as string;
  const context = await browser.newContext({ viewport: { width: 390, height: 844 } });
  const page = await context.newPage();
  try {
    await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), firstSecret);
    await page.goto('/analytics?tab=usage');
    await expect(page.getByRole('heading', { name: 'Usage Analytics' })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Save dashboard' })).toHaveCount(0);
    await page.getByLabel('Saved dashboard').click();
    await page.getByRole('option', { name: `${tag}-viewer` }).click();
    await expect(page.getByText('Requests by group', { exact: true })).toBeVisible();
    await expect(page.getByRole('alert')).toHaveCount(0);
    const scope = await request.post('/v0/management/analytics/query', { headers: { 'x-admin-key': firstSecret }, data: { start: Date.now() - 60_000, end: Date.now(), dimension: 'permissionGroup', metric: 'requests' } });
    expect(scope.status()).toBe(200);
    expect((await scope.json()).data.map((row: { label: string }) => row.label)).not.toContain(secondGroup);
  } finally {
    await context.close();
    await request.delete(`/v0/management/analytics/dashboards/${viewerDashboardId}`, { headers: { 'x-admin-key': adminKey } });
  }
});
