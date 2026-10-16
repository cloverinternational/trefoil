import { expect, test } from '@playwright/test';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

// Server-backed browser checks; never run against the Plexus reference.
test.skip(!process.env.TREFOIL_E2E_BASE_URL, 'Requires local Trefoil Compose');
const adminKey = readFileSync(resolve(import.meta.dirname, '../../.env'), 'utf8').match(/^ADMIN_KEY=(.*)$/m)?.[1]?.trim();

test.beforeEach(async ({ page }) => {
  if (!adminKey) throw new Error('Local admin key required');
  await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), adminKey);
});

test('Logs filters and details use actual backend records', async ({ page }) => {
  await page.goto('/logs');
  await expect(page.getByRole('heading', { name: 'Logs', exact: true }).last()).toBeVisible();
  await expect(page.getByRole('table').getByRole('row').count()).resolves.toBeGreaterThan(1);
  await expect(page.getByRole('alert')).toHaveCount(0);
  await page.getByRole('combobox', { name: 'Rows per page' }).click();
  await page.getByRole('option', { name: '50' }).click();
  await expect(page.getByRole('combobox', { name: 'Rows per page' })).toContainText('50');
  await page.getByPlaceholder('Model…').fill('not-a-real-model');
  await page.getByRole('button', { name: 'Search' }).click();
  await expect(page.getByText('No requests match these filters on this page.')).toBeVisible();
  await page.getByPlaceholder('Model…').fill('');
  await page.getByRole('button', { name: 'Search' }).click();
  await page.getByRole('table').getByRole('row').nth(1).click();
  await expect(page.getByRole('dialog', { name: 'Request details' })).toBeVisible();
});

test('Errors empty state and refresh retain the two-pane layout', async ({ page }) => {
  await page.goto('/errors');
  await expect(page.getByRole('heading', { name: 'Errors', exact: true }).last()).toBeVisible();
  await expect(page.getByRole('region', { name: 'Recent errors' })).toBeVisible();
  await expect(page.getByRole('region', { name: 'Error details' })).toContainText('Select an error to inspect details');
  await expect(page.getByRole('button', { name: 'Delete All' })).toBeDisabled();
  await page.getByRole('button', { name: 'Refresh' }).click();
  await expect(page.getByRole('region', { name: 'Recent errors' })).toContainText('No errors found.');
  await expect(page.getByRole('alert')).toHaveCount(0);
});

test('Mobile Logs and Errors controls remain reachable', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto('/logs');
  await expect(page.getByPlaceholder('Key…')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Search' })).toBeVisible();
  await page.goto('/errors');
  await expect(page.getByRole('button', { name: 'Refresh' })).toBeVisible();
  await expect(page.getByRole('region', { name: 'Error details' })).toBeVisible();
});
