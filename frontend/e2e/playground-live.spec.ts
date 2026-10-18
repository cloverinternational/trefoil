import { expect, test } from '@playwright/test';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

// Real browser against the local running stack; never intercepts inference.
test.skip(!process.env.TREFOIL_E2E_BASE_URL, 'Requires local Trefoil Compose service');
// Routable alias configured in that stack (sends one tiny real request).
const playgroundModel = process.env.TREFOIL_E2E_PLAYGROUND_MODEL ?? '';
const adminKey = readFileSync(resolve(import.meta.dirname, '../../.env'), 'utf8').match(/^ADMIN_KEY=(.*)$/m)?.[1]?.trim();

test('signed-in admin discovers a configured alias and sends optional settings', async ({ page }) => {
  test.skip(!playgroundModel, 'Set TREFOIL_E2E_PLAYGROUND_MODEL to a routable alias');
  if (!adminKey) throw new Error('Local admin credential unavailable');
  await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), adminKey);
  await page.goto('/playground');
  await expect(page.getByText('Test Configuration', { exact: true })).toBeVisible();
  await expect(page.getByText('Chat Simulation', { exact: true })).toBeVisible();
  await expect(page.getByText('Routing Decision', { exact: true })).toBeVisible();
  await expect(page.getByRole('combobox', { name: 'Model' })).toBeEnabled();
  await page.getByRole('combobox', { name: 'Model' }).click();
  await page.getByRole('option', { name: playgroundModel }).click();
  await page.getByText('Advanced request settings').click();
  await expect(page.getByRole('spinbutton', { name: 'Temperature (optional)' })).toHaveValue('');
  await page.getByRole('spinbutton', { name: 'Max tokens' }).fill('16');
  await page.getByRole('textbox', { name: 'Message' }).fill('Reply with one word: ready');
  const reply = page.waitForResponse(response => response.url().endsWith('/v1/chat/completions'));
  await page.getByRole('button', { name: 'Run request' }).click();
  const response = await reply;
  expect(response.status()).toBe(200);
  const sent = response.request().postDataJSON();
  expect(sent.model).toBe(playgroundModel);
  expect(sent.max_tokens).toBe(16);
  expect(sent).not.toHaveProperty('temperature');
  await expect(page.getByText('Assistant', { exact: true })).toBeVisible();
  await expect(page.getByRole('status')).toContainText('Routed');
  await expect(page.getByRole('alert')).toHaveCount(0);
});

test('narrow Playground retains card order and reachable composer', async ({ page }) => {
  if (!adminKey) throw new Error('Local admin credential unavailable');
  await page.setViewportSize({ width: 390, height: 844 });
  await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), adminKey);
  await page.goto('/playground');
  for (const title of ['Test Configuration', 'Chat Simulation', 'Routing Decision']) await expect(page.getByText(title, { exact: true })).toBeVisible();
  await expect(page.getByRole('combobox', { name: 'Model' })).toBeEnabled();
  await expect(page.getByRole('textbox', { name: 'Message' })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Run request' })).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
});

test('invalid settings and unknown model show errors rather than false success', async ({ page }) => {
  if (!adminKey) throw new Error('Local admin credential unavailable');
  await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), adminKey);
  await page.goto('/playground');
  await page.getByText('Advanced request settings').click();
  await page.getByRole('textbox', { name: 'Direct provider/model (optional)' }).fill('not-a-provider-model');
  await page.getByRole('button', { name: 'Run request' }).click();
  await expect(page.getByRole('alert')).toContainText('provider/model format');
  await page.getByRole('textbox', { name: 'Direct provider/model (optional)' }).fill('missing-provider/missing-model');
  await page.getByRole('spinbutton', { name: 'Max tokens' }).fill('16');
  await page.getByRole('textbox', { name: 'Message' }).fill('Reply ready');
  const reply = page.waitForResponse(response => response.url().endsWith('/v1/chat/completions'));
  await page.getByRole('button', { name: 'Run request' }).click();
  expect((await reply).status()).toBe(404);
  await expect(page.getByRole('alert')).toBeVisible();
  await expect(page.getByText('Assistant', { exact: true })).toHaveCount(0);
});
