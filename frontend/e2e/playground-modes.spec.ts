import { expect, test } from '@playwright/test';

const modes = [
  { name: 'chat', path: '/v1/chat/completions', body: { choices: [{ message: { content: 'chat answer' } }] } },
  { name: 'messages', path: '/v1/messages', body: { content: [{ type: 'text', text: 'messages answer' }] } },
  { name: 'responses', path: '/v1/responses', body: { output: [{ content: [{ type: 'output_text', text: 'responses answer' }] }] } },
  { name: 'Gemini', path: '/v1beta/models/test-model:generateContent', body: { candidates: [{ content: { parts: [{ text: 'Gemini answer' }] } }] } },
];

test('API modes send their wire format and show returned text', async ({ page }) => {
  await page.route('**/v0/management/**', route => route.fulfill({ json: {} }));
  await page.route('**/v0/management/auth/verify', route => route.fulfill({ json: { ok: true, role: 'limited', keyName: 'browser-test', allowedModels: [], allowedProviders: [] } }));
  await page.addInitScript(() => localStorage.setItem('trefoil_admin_key', 'browser-test-key'));
  await page.route('**/v0/management/config/status', route => route.fulfill({ json: { providerCount: 0, modelAliasCount: 0, keyCount: 0, quotaCount: 0, mcpServerCount: 0 } }));
  await page.route('**/v1/models', route => route.fulfill({ json: { data: [{ id: 'test-model' }] } }));
  await page.goto('/playground');
  await expect(page.getByRole('heading', { name: 'Playground' }).last()).toBeVisible();

  for (const mode of modes) {
    let submitted: Record<string, unknown> | undefined;
    let authHeader: string | undefined;
    await page.route(`**${mode.path}`, async route => {
      submitted = route.request().postDataJSON();
      const headers = route.request().headers();
      authHeader = headers['x-api-key'] ?? headers['x-goog-api-key'];
      await route.fulfill({ json: mode.body });
    });
    await expect(page.getByRole('combobox', { name: 'API' })).toBeEnabled();
    await page.getByRole('combobox', { name: 'API' }).click();
    await page.getByRole('option', { name: mode.name, exact: true }).click();
    await page.getByRole('textbox', { name: 'Message' }).fill('hello');
    await page.getByRole('button', { name: 'Run request' }).click();
    await expect(page.getByText(`${mode.name} answer`, { exact: false }).first()).toBeVisible();
    expect(authHeader).toBe('browser-test-key');
    if (mode.name === 'messages') expect(submitted).toMatchObject({ model: 'test-model', max_tokens: 128 });
    if (mode.name === 'responses') expect(submitted).toMatchObject({ model: 'test-model', input: 'hello', max_output_tokens: 128 });
    if (mode.name === 'Gemini') expect(submitted).toMatchObject({ contents: [{ role: 'user', parts: [{ text: 'hello' }] }] });
    await page.unroute(`**${mode.path}`);
  }
});

test('API mode reports HTTP and malformed success responses honestly', async ({ page }) => {
  await page.route('**/v0/management/**', route => route.fulfill({ json: {} }));
  await page.route('**/v0/management/auth/verify', route => route.fulfill({ json: { ok: true, role: 'limited', keyName: 'browser-test', allowedModels: [], allowedProviders: [] } }));
  await page.addInitScript(() => localStorage.setItem('trefoil_admin_key', 'browser-test-key'));
  await page.route('**/v0/management/config/status', route => route.fulfill({ json: { providerCount: 0, modelAliasCount: 0, keyCount: 0, quotaCount: 0, mcpServerCount: 0 } }));
  await page.route('**/v1/models', route => route.fulfill({ json: { data: [{ id: 'test-model' }] } }));
  await page.goto('/playground');
  await expect(page.getByRole('heading', { name: 'Playground' }).last()).toBeVisible();
  await page.route('**/v1/messages', route => route.fulfill({ status: 502, json: { error: { message: 'upstream unavailable' } } }));
  await page.getByRole('combobox', { name: 'API' }).click();
  await page.getByRole('option', { name: 'messages', exact: true }).click();
  await page.getByRole('textbox', { name: 'Message' }).fill('hello');
  await page.getByRole('button', { name: 'Run request' }).click();
  await expect(page.getByRole('alert')).toContainText('upstream unavailable');
  await page.unroute('**/v1/messages');
  await page.route('**/v1/responses', route => route.fulfill({ json: { output: [] } }));
  await page.getByRole('combobox', { name: 'API' }).click();
  await page.getByRole('option', { name: 'responses', exact: true }).click();
  await page.getByRole('button', { name: 'Run request' }).click();
  await expect(page.getByRole('alert')).toContainText('returned no assistant text');
  await expect(page.getByRole('status')).toContainText('Failed');
});
