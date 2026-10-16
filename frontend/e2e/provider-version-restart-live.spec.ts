import { expect, test } from '@playwright/test';
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { randomUUID } from 'node:crypto';
import { resolve } from 'node:path';

test.skip(!process.env.TREFOIL_E2E_BASE_URL, 'Requires local Trefoil Compose');
test.describe.configure({ mode: 'serial' });
const admin = readFileSync(resolve(import.meta.dirname, '../../.env'), 'utf8').match(/^ADMIN_KEY=(.*)$/m)?.[1]?.trim();

test('provider revision survives local backend restart and stale write still conflicts', async ({ request }) => {
  if (!admin) throw new Error('Local admin key required');
  const name = `restart-e2e-${randomUUID().replaceAll('-', '').slice(0, 10)}`;
  const headers = { 'x-admin-key': admin };
  try {
    expect((await request.put(`/v0/management/providers/${name}`, { headers, data: { api_base_url: 'https://invalid.example.test/v1', api_key: 'disposable', display_name: 'Before', enabled: true } })).status()).toBe(200);
    const initial = (await (await request.get('/v0/management/providers', { headers })).json())[name];
    expect((await request.patch(`/v0/management/providers/${name}`, { headers, data: { expectedVersion: initial.version, display_name: 'After' } })).status()).toBe(200);
    const changed = (await (await request.get('/v0/management/providers', { headers })).json())[name];
    expect(changed.version).toBe(initial.version + 1);
    execFileSync('docker', ['compose', 'restart', 'backend'], { cwd: resolve(import.meta.dirname, '../..'), stdio: 'pipe', timeout: 30_000 });
    await expect.poll(async () => (await request.get('/healthz')).status(), { timeout: 30_000 }).toBe(200);
    const reloaded = (await (await request.get('/v0/management/providers', { headers })).json())[name];
    expect(reloaded.version).toBe(changed.version);
    expect(reloaded.display_name).toBe('After');
    expect((await request.patch(`/v0/management/providers/${name}`, { headers, data: { expectedVersion: initial.version, display_name: 'Stale' } })).status()).toBe(409);
    expect((await (await request.get('/v0/management/providers', { headers })).json())[name].display_name).toBe('After');
  } finally {
    await request.delete(`/v0/management/providers/${name}`, { headers });
  }
});
