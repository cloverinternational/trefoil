import { expect, test } from '@playwright/test';
import type { Page, Route } from '@playwright/test';

const models = {
  fast: { type: 'chat', additional_aliases: ['quick'], target_groups: [{ targets: [{ provider: 'fixture', model: 'fixture-chat' }] }] },
};
const usage = { data: [{ requestId: 'req-1', date: '2026-01-01', provider: 'fixture', incomingModelAlias: 'fast', responseStatus: 'success', startTime: 1767225600, tokensInput: 3, tokensOutput: 5, costTotal: 0.12, durationMs: 20 }], total: 1 };

async function fixture(page: Page, options: { auth?: boolean; failModels?: boolean } = {}) {
  let providers: Record<string, Record<string, unknown>> = {
    fixture: { api_base_url: 'https://fixture.invalid', enabled: true, api_key: 'configured' },
  };
  await page.route('**/v0/**', async (route: Route) => {
    const request = route.request();
    const path = new URL(request.url()).pathname;
    const method = request.method();
    if (path.endsWith('/auth/verify')) return route.fulfill({ json: options.auth === false ? { ok: false } : { ok: true, role: 'admin' } });
    if (path.endsWith('/aliases') && method === 'GET') {
      if (options.failModels) return route.fulfill({ status: 503, json: { error: 'fixture unavailable' } });
      return route.fulfill({ json: models });
    }
    if (path.endsWith('/usage/summary')) return route.fulfill({ json: { stats: { totalRequests: 1, totalErrors: 0, inputTokens: 3, outputTokens: 5, totalCost: 0.12 }, today: { requests: 1, totalCost: 0.12 }, series: [] } });
    if (path.endsWith('/usage')) return route.fulfill({ json: usage });
    if (path.endsWith('/concurrency')) return route.fulfill({ json: { data: [] } });
    if (path.endsWith('/cooldowns')) return route.fulfill({ json: [] });
    if (path.endsWith('/metrics')) return route.fulfill({ contentType: 'text/plain', body: 'plexus_requests_total 1\n' });
    if (path.endsWith('/config/status')) return route.fulfill({ json: { providerCount: Object.keys(providers).length, modelAliasCount: 1, keyCount: 0, quotaCount: 0, mcpServerCount: 0 } });
    if (path.endsWith('/providers') && method === 'GET') return route.fulfill({ json: providers });
    if (path.includes('/providers/') && method === 'PUT') {
      const name = decodeURIComponent(path.split('/').pop()!);
      providers[name] = { ...JSON.parse(request.postData() ?? '{}'), api_key: 'configured' };
      return route.fulfill({ status: 204 });
    }
    if (path.endsWith('/oauth/providers')) return route.fulfill({ json: { data: [] } });
    if (path.endsWith('/logging/level')) return route.fulfill({ json: { level: 'info', startupLevel: 'info', supportedLevels: ['info'] } });
    if (path.endsWith('/logging/modules')) return route.fulfill({ json: { modules: [] } });
    if (path.endsWith('/system/logs/stream')) return route.fulfill({ status: 200, contentType: 'text/event-stream', body: 'event: syslog\ndata: {"level":"info","message":"first","timestamp":"1"}\n\nevent: syslog\ndata: {"level":"info","message":"second","timestamp":"2"}\n\n' });
    throw new Error(`Unhandled mocked request: ${method} ${path}`);
  });
}

test('auth boundaries, navigation, and model search', async ({ page }) => {
  await fixture(page, { auth: false });
  await page.goto('/models');
  await expect(page.getByText('Sign in', { exact: true })).toBeVisible();
  await page.getByLabel(/admin key/i).fill('bad');
  await page.getByRole('button', { name: /access dashboard/i }).click();
  await expect(page.getByText(/Invalid key/)).toBeVisible();
  await page.unrouteAll();
  await fixture(page);
  await page.getByLabel(/admin key/i).fill('fixture-admin');
  await page.getByRole('button', { name: /access dashboard/i }).click();
  await expect(page).toHaveURL(/\/models$/);
  await page.getByRole('link', { name: 'Models' }).click();
  await expect(page.getByRole('cell', { name: /fast/ })).toBeVisible();
  await page.getByLabel('Search models').fill('missing');
  await expect(page.getByText('No models match your search')).toBeVisible();
});

test('usage records and API error state', async ({ page }) => {
  await fixture(page);
  await page.addInitScript(() => localStorage.setItem('trefoil_admin_key', 'fixture-admin'));
  await page.goto('/usage');
  await expect(page.getByText('req-1')).toBeVisible();
  await page.unrouteAll();
  await fixture(page, { failModels: true });
  await page.goto('/models');
  await expect(page.getByRole('alert')).toContainText(/Unable to load model aliases/);
});

test('provider CRUD state and ordered SSE stream', async ({ page }) => {
  await fixture(page);
  await page.addInitScript(() => localStorage.setItem('trefoil_admin_key', 'fixture-admin'));
  await page.goto('/providers');
  await expect(page.getByText('https://fixture.invalid')).toBeVisible();
  await page.getByRole('button', { name: /add provider/i }).click();
  const dialog = page.getByRole('dialog');
  await expect(dialog).toBeVisible();
  await dialog.getByRole('button', { name: /API key provider/i }).click();
  await dialog.getByLabel('Provider ID').fill('new-provider');
  await dialog.getByLabel('API base URL').fill('https://new.invalid');
  await dialog.getByLabel(/API key/).fill('secret');
  await dialog.getByRole('button', { name: 'Save provider' }).click();
  await expect(dialog).toHaveCount(0);
  await expect(page.getByText('https://new.invalid')).toBeVisible();
  await page.goto('/system-logs');
  await expect(page.getByText('first')).toBeVisible();
  await expect(page.getByText('second')).toBeVisible();
  const first = page.getByText('first', { exact: false }).last();
  const second = page.getByText('second', { exact: false }).last();
  await expect(first).toBeVisible();
  await expect(second).toBeVisible();
  expect(await first.evaluate((node, other) => !!(node.compareDocumentPosition(other) & Node.DOCUMENT_POSITION_FOLLOWING), await second.elementHandle())).toBe(true);
});

test('dashboard shell collapses persistently and dashboard links reach real pages', async ({ page }) => {
  await fixture(page);
  await page.addInitScript(() => localStorage.setItem('trefoil_admin_key', 'fixture-admin'));
  await page.goto('/');
  await expect(page.getByRole('heading', { name: 'Dashboard' })).toBeVisible();
  await expect(page.getByRole('heading', { name: 'Live Metrics' })).toBeVisible();
  await page.getByRole('button', { name: 'Collapse sidebar' }).click();
  await expect(page.getByRole('button', { name: 'Expand sidebar' })).toBeVisible();
  await page.reload();
  await expect(page.getByRole('button', { name: 'Expand sidebar' })).toBeVisible();
  await page.getByRole('link', { name: 'Usage Analytics' }).click();
  await expect(page).toHaveURL(/\/usage$/);
  await page.goto('/');
  await page.getByRole('link', { name: 'Performance' }).click();
  await expect(page).toHaveURL(/\/metrics$/);
});

test('dashboard navigation remains usable in mobile drawer', async ({ page }) => {
  await fixture(page);
  await page.addInitScript(() => localStorage.setItem('trefoil_admin_key', 'fixture-admin'));
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto('/');
  await page.getByRole('button', { name: 'Open navigation' }).click();
  await expect(page.getByRole('button', { name: 'Close navigation' })).toBeVisible();
  await page.getByRole('link', { name: 'Providers' }).last().click();
  await expect(page).toHaveURL(/\/providers$/);
  await expect(page.getByRole('button', { name: 'Close navigation' })).toBeHidden();
});
