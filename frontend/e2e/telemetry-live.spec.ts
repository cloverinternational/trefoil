import { expect, test } from '@playwright/test';
import { createServer } from 'node:http';
import { randomUUID } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

test.skip(!process.env.TREFOIL_E2E_BASE_URL, 'Requires local Trefoil Compose');
const admin = readFileSync(resolve(import.meta.dirname, '../../.env'), 'utf8').match(/^ADMIN_KEY=(.*)$/m)?.[1]?.trim();

test('real streamed request records TTFT and TPS; nonstream request leaves TTFT unknown', async ({ page, request }) => {
  if (!admin) throw new Error('Local admin key required');
  const provider = `telemetry-e2e-${randomUUID().replaceAll('-', '').slice(0, 10)}`;
  const headers = { 'x-admin-key': admin };
  const server = createServer((req, res) => {
    let text = '';
    req.on('data', chunk => { text += chunk; });
    req.on('end', () => {
      if (req.url === '/responses') { if ((JSON.parse(text) as { stream?: boolean }).stream) { res.writeHead(200, { 'content-type': 'text/event-stream' }); res.write('data: {"type":"response.created"}\n\n'); setTimeout(() => { res.write('data: {"type":"response.output_text.delta","delta":"hello"}\n\n'); setTimeout(() => { res.end('data: {"type":"response.completed","response":{"usage":{"input_tokens":2,"output_tokens":6}}}\n\n'); }, 90); }, 90); } else { res.writeHead(200, { 'content-type': 'application/json' }); res.end(JSON.stringify({ id: `response-${provider}`, object: 'response', status: 'completed', model: 'sample', output: [], usage: { input_tokens: 2, output_tokens: 6 } })); } return; }
      if (req.url !== '/chat/completions') { res.writeHead(404).end(); return; }
      if ((JSON.parse(text) as { model?: string }).model === 'abort') { res.writeHead(200, { 'content-type': 'text/event-stream' }); res.write('data: {"choices":[{"delta":{"content":"partial"}}]}\n\n'); setTimeout(() => { if (!res.destroyed) res.write('data: {"usage":{"prompt_tokens":2,"completion_tokens":6}}\n\n'); }, 3000); return; }
      if ((JSON.parse(text) as { model?: string }).model === 'failure') { res.writeHead(200, { 'content-type': 'text/event-stream' }); res.write('data: {"choices":[{"delta":{"content":"partial"}}]}\n\n'); setTimeout(() => { res.write('data: {"usage":{"prompt_tokens":2,"completion_tokens":6}}\n\n'); res.end('data: {"type":"response.failed"}\n\n'); }, 80); return; }
      if ((JSON.parse(text) as { stream?: boolean }).stream) { res.writeHead(200, { 'content-type': 'text/event-stream' }); res.write('data: {"choices":[{"delta":{"role":"assistant"}}]}\n\n'); setTimeout(() => { res.write('data: {"choices":[{"delta":{"content":"hello"}}]}\n\n'); setTimeout(() => { res.write('data: {"usage":{"prompt_tokens":2,"completion_tokens":6}}\n\n'); res.end('data: [DONE]\n\n'); }, 90); }, 90); }
      else { res.writeHead(200, { 'content-type': 'application/json' }); res.end(JSON.stringify({ id: 'telemetry', choices: [{ message: { role: 'assistant', content: 'hello' }, finish_reason: 'stop' }], usage: { prompt_tokens: 2, completion_tokens: 6 } })); }
    });
  });
  await new Promise<void>(done => server.listen(0, '0.0.0.0', done));
  try {
    const origin = `http://192.168.96.1:${(server.address() as { port: number }).port}`;
    expect((await request.put(`/v0/management/providers/${provider}`, { headers, data: { api_base_url: origin, api_key: 'disposable', enabled: true } })).status()).toBe(200);
    await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), admin);
    const streamed = await request.post('/v1/chat/completions', { headers: { 'x-api-key': admin }, data: { model: `${provider}/sample`, stream: true, messages: [{ role: 'user', content: 'hello' }] } });
    expect(streamed.status()).toBe(200);
    await streamed.body();
    const nonstream = await request.post('/v1/chat/completions', { headers: { 'x-api-key': admin }, data: { model: `${provider}/sample`, messages: [{ role: 'user', content: 'hello' }] } });
    expect(nonstream.status()).toBe(200);
    const usage = await request.get('/v0/management/usage?limit=20&offset=0', { headers });
    expect(usage.status()).toBe(200);
    const records = ((await usage.json()).data as { requestId: string; provider: string; ttftMs: number | null; tokensPerSec: number | null; e2eTokensPerSec: number | null; responseStatus: string }[]).filter(record => record.provider === provider);
    expect(records).toHaveLength(2);
    const stream = records.find(record => record.ttftMs !== null);
    expect(stream?.responseStatus).toBe('success');
    expect(stream?.ttftMs).toBeGreaterThan(0);
    expect(stream?.tokensPerSec).toBeGreaterThan(0);
    expect(stream?.e2eTokensPerSec).toBeGreaterThan(0);
    expect(stream?.tokensPerSec).toBeGreaterThan(stream!.e2eTokensPerSec!);
    const plain = records.find(record => record.ttftMs === null);
    expect(plain?.tokensPerSec).toBeNull();
    expect(plain?.e2eTokensPerSec).toBeGreaterThan(0);
    const responses = await request.post('/v1/responses', { headers: { 'x-api-key': admin }, data: { model: `${provider}/sample`, input: 'hello' } });
    expect(responses.status()).toBe(200);
    const responseUsage = await request.get('/v0/management/usage?limit=20&offset=0', { headers });
    const responseRecords = ((await responseUsage.json()).data as { requestId: string; provider: string; responseStatus: string; e2eTokensPerSec: number | null }[]).filter(record => record.provider === provider);
    expect(responseRecords).toHaveLength(3);
    expect(responseRecords.find(record => record.requestId !== stream?.requestId && record.requestId !== plain?.requestId)?.e2eTokensPerSec).toBeGreaterThan(0);
    const streamedResponses = await request.post('/v1/responses', { headers: { 'x-api-key': admin }, data: { model: `${provider}/sample`, input: 'hello', stream: true } });
    expect(streamedResponses.status()).toBe(200);
    await streamedResponses.body();
    await expect.poll(async () => {
      const reply = await request.get('/v0/management/usage?limit=20&offset=0', { headers });
      return ((await reply.json()).data as { provider: string; tokensPerSec: number | null; ttftMs: number | null }[]).filter(record => record.provider === provider && record.ttftMs !== null && record.tokensPerSec !== null).length;
    }, { timeout: 5000 }).toBe(2);
    const streamedUsage = await request.get('/v0/management/usage?limit=20&offset=0', { headers });
    const streamedRows = ((await streamedUsage.json()).data as { provider: string; ttftMs: number | null; tokensPerSec: number | null; e2eTokensPerSec: number | null }[]).filter(row => row.provider === provider && row.ttftMs !== null);
    expect(streamedRows).toHaveLength(2);
    for (const row of streamedRows) {
      expect(row.e2eTokensPerSec).toBeGreaterThan(0);
      expect(row.tokensPerSec).toBeGreaterThan(row.e2eTokensPerSec!);
    }
    const failed = await request.post('/v1/chat/completions', { headers: { 'x-api-key': admin }, data: { model: `${provider}/failure`, stream: true, messages: [{ role: 'user', content: 'hello' }] } });
    expect(failed.status()).toBe(200);
    await failed.body();
    await expect.poll(async () => {
      const failedUsage = await request.get('/v0/management/usage?limit=20&offset=0', { headers });
      return ((await failedUsage.json()).data as { provider: string; responseStatus: string }[]).filter(record => record.provider === provider && record.responseStatus === 'error').length;
    }, { timeout: 5000 }).toBe(1);
    const failedUsage = await request.get('/v0/management/usage?limit=20&offset=0', { headers });
    const failedRows = ((await failedUsage.json()).data as { provider: string; responseStatus: string; tokensPerSec: number | null; ttftMs: number | null; e2eTokensPerSec: number | null }[]).filter(record => record.provider === provider && record.responseStatus === 'error');
    expect(failedRows).toHaveLength(1);
    expect(failedRows[0].tokensPerSec).toBeNull();
    expect(failedRows[0].ttftMs).toBeNull();
    expect(failedRows[0].e2eTokensPerSec).toBeNull();
    const performance = await request.get(`/v0/management/performance?provider=${provider}`, { headers });
    expect(performance.status()).toBe(200);
    const samples = await performance.json() as { provider: string; sample_count: number; ttft_sample_count: number; tps_sample_count: number; e2e_sample_count: number; avg_ttft_ms: number; avg_tokens_per_sec: number; avg_e2e_tokens_per_sec: number }[];
    const sample = samples.find(item => item.provider === provider);
    expect(sample?.sample_count).toBe(4);
    expect(sample?.ttft_sample_count).toBe(2);
    expect(sample?.tps_sample_count).toBe(2);
    expect(sample?.e2e_sample_count).toBe(4);
    expect(sample?.avg_ttft_ms).toBeGreaterThan(0);
    expect(sample?.avg_tokens_per_sec).toBeGreaterThan(0);
    expect(sample?.avg_e2e_tokens_per_sec).toBeGreaterThan(0);
    const summary = await request.get('/v0/management/usage/summary?range=day', { headers });
    expect(summary.status()).toBe(200);
    const totals = await summary.json() as { stats: { ttftSamples: number; tpsSamples: number }; series: { ttftSamples: number; tpsSamples: number }[] };
    expect(totals.stats.ttftSamples).toBeGreaterThanOrEqual(2);
    expect(totals.stats.tpsSamples).toBeGreaterThanOrEqual(2);
    expect(totals.series.some(bucket => bucket.ttftSamples >= 2 && bucket.tpsSamples >= 2)).toBe(true);
    const end = Date.now() + 1000;
    const allUsage = await request.get('/v0/management/usage?limit=20&offset=0', { headers });
    const measured = ((await allUsage.json()).data as { provider: string; ttftMs: number | null; tokensPerSec: number | null }[]).filter(record => record.provider === provider && record.ttftMs !== null && record.tokensPerSec !== null);
    for (const [metric, expected] of [['avgTps', measured.reduce((sum, record) => sum + record.tokensPerSec!, 0) / measured.length], ['avgTtftMs', measured.reduce((sum, record) => sum + record.ttftMs!, 0) / measured.length]] as const) {
      const query = await request.post('/v0/management/analytics/query', { headers, data: { start: end - 60_000, end: Date.now(), dimension: 'provider', metric, filters: [{ dimension: 'provider', value: provider }] } });
      expect(query.status()).toBe(200);
      const point = (await query.json()).data.find((row: { label: string }) => row.label === provider) as { value: number; samples: number } | undefined;
      expect(point?.samples).toBe(2);
      expect(point?.value).toBeCloseTo(expected, 2);
    }
    await page.goto('/logs');
    await expect(page.getByText(`TTFT: ${stream?.ttftMs}`, { exact: false }).first()).toBeVisible();
    await expect(page.getByText('TPS:', { exact: false }).first()).toBeVisible();
    await page.goto('/analytics?tab=performance');
    await expect(page.getByRole('heading', { name: 'Performance' }).last()).toBeVisible();
    await expect(page.getByText(provider, { exact: true }).first()).toBeVisible();
    await page.getByRole('textbox', { name: 'Provider filter' }).fill(provider);
    const samplesTable = page.getByRole('heading', { name: 'Provider performance samples' }).locator('..').locator('..');
    await expect(samplesTable).toContainText(`${provider} / ${provider}/sample`);
    await expect(samplesTable).toContainText('2 measured');
    await expect(samplesTable).toContainText('4 measured');
  } finally {
    await request.delete(`/v0/management/providers/${provider}`, { headers });
    // Fixture IDs are UUID-derived ASCII. Remove only this run's telemetry rows.
    execFileSync('docker', ['compose', 'exec', '-T', 'postgres', 'psql', '-U', 'trefoil', '-d', 'trefoil', '-v', 'ON_ERROR_STOP=1', '-q', '-c', `DELETE FROM provider_performance WHERE provider='${provider}'; DELETE FROM request_usage WHERE provider='${provider}';`], { cwd: resolve(import.meta.dirname, '../..'), stdio: 'pipe', timeout: 15_000 });
    await new Promise<void>(done => server.close(() => done()));
  }
});

test('native Gemini stream records measured TPS from content then terminal usage', async ({ page, request }) => {
  if (!admin) throw new Error('Local admin key required');
  const provider = `gemini-telemetry-e2e-${randomUUID().replaceAll('-', '').slice(0, 8)}`;
  const headers = { 'x-admin-key': admin };
  const server = createServer((req, res) => {
    req.resume();
    req.on('end', () => {
      if (req.url !== '/v1beta/models/sample:streamGenerateContent') { res.writeHead(404).end(); return; }
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      res.write('data: {"candidates":[{"content":{"parts":[{"text":"hello"}]}}]}\n\n');
      setTimeout(() => res.end('data: {"usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":6}}\n\n'), 95);
    });
  });
  await new Promise<void>(done => server.listen(0, '0.0.0.0', done));
  try {
    const origin = `http://192.168.96.1:${(server.address() as { port: number }).port}`;
    expect((await request.put(`/v0/management/providers/${provider}`, { headers, data: { api_base_url: origin, api_key: 'disposable', enabled: true } })).status()).toBe(200);
    await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), admin);
    const response = await request.post(`/v1beta/models/${provider}%2Fsample:streamGenerateContent`, { headers: { 'x-api-key': admin }, data: { contents: [{ parts: [{ text: 'hello' }] }], stream: true } });
    expect(response.status()).toBe(200);
    await response.body();
    await expect.poll(async () => {
      const usage = await request.get('/v0/management/usage?limit=20&offset=0', { headers });
      return ((await usage.json()).data as { provider: string; responseStatus: string; ttftMs: number | null; tokensPerSec: number | null }[]).filter(row => row.provider === provider && row.responseStatus === 'success' && row.ttftMs !== null && row.tokensPerSec !== null).length;
    }, { timeout: 5000 }).toBe(1);
  } finally {
    await request.delete(`/v0/management/providers/${provider}`, { headers });
    execFileSync('docker', ['compose', 'exec', '-T', 'postgres', 'psql', '-U', 'trefoil', '-d', 'trefoil', '-v', 'ON_ERROR_STOP=1', '-q', '-c', `DELETE FROM provider_performance WHERE provider='${provider}'; DELETE FROM request_usage WHERE provider='${provider}';`], { cwd: resolve(import.meta.dirname, '../..'), stdio: 'pipe', timeout: 15_000 });
    await new Promise<void>(done => server.close(() => done()));
  }
});

test('client abort after partial output leaves no successful throughput sample', async ({ page, request }) => {
  if (!admin) throw new Error('Local admin key required');
  const provider = `abort-telemetry-e2e-${randomUUID().replaceAll('-', '').slice(0, 8)}`;
  const headers = { 'x-admin-key': admin };
  const server = createServer((req, res) => {
    req.resume();
    req.on('end', () => {
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      res.write('data: {"choices":[{"delta":{"content":"partial"}}]}\n\n');
      setTimeout(() => { if (!res.destroyed) res.end('data: [DONE]\n\n'); }, 3000);
    });
  });
  await new Promise<void>(done => server.listen(0, '0.0.0.0', done));
  try {
    const origin = `http://192.168.96.1:${(server.address() as { port: number }).port}`;
    expect((await request.put(`/v0/management/providers/${provider}`, { headers, data: { api_base_url: origin, api_key: 'disposable', enabled: true } })).status()).toBe(200);
    await page.addInitScript(key => localStorage.setItem('trefoil_admin_key', key), admin);
    await page.goto('/logs');
    const result = await page.evaluate(async ({ provider, credential }) => {
      const controller = new AbortController();
      const response = await fetch('/v1/chat/completions', { method: 'POST', signal: controller.signal, headers: { 'content-type': 'application/json', 'x-api-key': credential }, body: JSON.stringify({ model: `${provider}/sample`, stream: true, messages: [{ role: 'user', content: 'hello' }] }) });
      const reader = response.body!.getReader();
      await reader.read();
      const id = response.headers.get('x-request-id');
      controller.abort();
      await reader.cancel().catch(() => undefined);
      return { status: response.status, id };
    }, { provider, credential: admin });
    expect(result.status).toBe(200);
    expect(result.id).toBeTruthy();
    await expect.poll(async () => {
      const usage = await request.get('/v0/management/usage?limit=20&offset=0', { headers });
      return ((await usage.json()).data as { requestId: string; responseStatus: string }[]).find(row => row.requestId === result.id)?.responseStatus;
    }, { timeout: 5000 }).toBe('cancelled');
    const usage = await request.get('/v0/management/usage?limit=20&offset=0', { headers });
    const row = ((await usage.json()).data as { requestId: string; ttftMs: number | null; tokensPerSec: number | null; e2eTokensPerSec: number | null }[]).find(row => row.requestId === result.id);
    expect(row?.ttftMs).toBeNull();
    expect(row?.tokensPerSec).toBeNull();
    expect(row?.e2eTokensPerSec).toBeNull();
    const performance = await request.get(`/v0/management/performance?provider=${provider}`, { headers });
    expect((await performance.json())).toHaveLength(0);
  } finally {
    await request.delete(`/v0/management/providers/${provider}`, { headers });
    execFileSync('docker', ['compose', 'exec', '-T', 'postgres', 'psql', '-U', 'trefoil', '-d', 'trefoil', '-v', 'ON_ERROR_STOP=1', '-q', '-c', `DELETE FROM provider_performance WHERE provider='${provider}'; DELETE FROM request_usage WHERE provider='${provider}';`], { cwd: resolve(import.meta.dirname, '../..'), stdio: 'pipe', timeout: 15_000 });
    server.closeAllConnections();
    await new Promise<void>(done => server.close(() => done()));
  }
});

test('upstream transport interruption after partial text records error without throughput', async ({ request }) => {
  if (!admin) throw new Error('Local admin key required');
  const provider = `interrupt-telemetry-e2e-${randomUUID().replaceAll('-', '').slice(0, 8)}`;
  const headers = { 'x-admin-key': admin };
  const server = createServer((req, res) => {
    req.resume();
    req.on('end', () => {
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      res.write('data: {"choices":[{"delta":{"content":"partial"}}]}\n\n');
      setTimeout(() => res.socket?.destroy(), 90);
    });
  });
  await new Promise<void>(done => server.listen(0, '0.0.0.0', done));
  try {
    const origin = `http://192.168.96.1:${(server.address() as { port: number }).port}`;
    expect((await request.put(`/v0/management/providers/${provider}`, { headers, data: { api_base_url: origin, api_key: 'disposable', enabled: true } })).status()).toBe(200);
    // An interrupted chunked response may make Playwright's API context reject
    // the entire request even after headers; the persisted row is authoritative.
    await request.post('/v1/chat/completions', { headers: { 'x-api-key': admin }, data: { model: `${provider}/sample`, stream: true, messages: [{ role: 'user', content: 'hello' }] } }).catch(() => undefined);
    await expect.poll(async () => {
      const usage = await request.get('/v0/management/usage?limit=20&offset=0', { headers });
      return ((await usage.json()).data as { provider: string; responseStatus: string }[]).find(row => row.provider === provider)?.responseStatus;
    }, { timeout: 5000 }).toBe('error');
    const usage = await request.get('/v0/management/usage?limit=20&offset=0', { headers });
    const row = ((await usage.json()).data as { provider: string; ttftMs: number | null; tokensPerSec: number | null; e2eTokensPerSec: number | null }[]).find(row => row.provider === provider);
    expect(row).toMatchObject({ ttftMs: null, tokensPerSec: null, e2eTokensPerSec: null });
    const performance = await request.get(`/v0/management/performance?provider=${provider}`, { headers });
    expect((await performance.json())).toHaveLength(0);
  } finally {
    await request.delete(`/v0/management/providers/${provider}`, { headers });
    execFileSync('docker', ['compose', 'exec', '-T', 'postgres', 'psql', '-U', 'trefoil', '-d', 'trefoil', '-v', 'ON_ERROR_STOP=1', '-q', '-c', `DELETE FROM provider_performance WHERE provider='${provider}'; DELETE FROM request_usage WHERE provider='${provider}';`], { cwd: resolve(import.meta.dirname, '../..'), stdio: 'pipe', timeout: 15_000 });
    server.closeAllConnections();
    await new Promise<void>(done => server.close(() => done()));
  }
});

test('a new request after a long partial stream is independent, not an inferred reconnect', async ({ request }) => {
  if (!admin) throw new Error('Local admin key required');
  const provider = `resume-telemetry-e2e-${randomUUID().replaceAll('-', '').slice(0, 8)}`;
  const headers = { 'x-admin-key': admin };
  let attempts = 0;
  const server = createServer((req, res) => {
    req.resume();
    req.on('end', () => {
      attempts++;
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      res.write('data: {"choices":[{"delta":{"content":"first"}}]}\n\n');
      if (attempts === 1) setTimeout(() => res.socket?.destroy(), 450);
      else setTimeout(() => res.end('data: {"usage":{"prompt_tokens":2,"completion_tokens":4}}\n\ndata: [DONE]\n\n'), 95);
    });
  });
  await new Promise<void>(done => server.listen(0, '0.0.0.0', done));
  try {
    const origin = `http://192.168.96.1:${(server.address() as { port: number }).port}`;
    expect((await request.put(`/v0/management/providers/${provider}`, { headers, data: { api_base_url: origin, api_key: 'disposable', enabled: true } })).status()).toBe(200);
    const data = { model: `${provider}/sample`, stream: true, messages: [{ role: 'user', content: 'hello' }] };
    await request.post('/v1/chat/completions', { headers: { 'x-api-key': admin }, data }).catch(() => undefined);
    await expect.poll(async () => {
      const usage = await request.get('/v0/management/usage?limit=20&offset=0', { headers });
      return ((await usage.json()).data as { provider: string; responseStatus: string }[]).filter(row => row.provider === provider).map(row => row.responseStatus);
    }, { timeout: 5000 }).toEqual(['error']);
    expect(attempts).toBe(1);
    const next = await request.post('/v1/chat/completions', { headers: { 'x-api-key': admin }, data });
    expect(next.status()).toBe(200);
    await next.body();
    await expect.poll(async () => {
      const usage = await request.get('/v0/management/usage?limit=20&offset=0', { headers });
      return ((await usage.json()).data as { provider: string; responseStatus: string }[]).filter(row => row.provider === provider).map(row => row.responseStatus).sort();
    }, { timeout: 5000 }).toEqual(['error', 'success']);
    expect(attempts).toBe(2);
    const usage = await request.get('/v0/management/usage?limit=20&offset=0', { headers });
    const rows = ((await usage.json()).data as { provider: string; responseStatus: string; tokensPerSec: number | null; ttftMs: number | null }[]).filter(row => row.provider === provider);
    expect(rows.find(row => row.responseStatus === 'error')).toMatchObject({ tokensPerSec: null, ttftMs: null });
    expect(rows.find(row => row.responseStatus === 'success')?.tokensPerSec).toBeGreaterThan(0);
    const perf = await request.get(`/v0/management/performance?provider=${provider}`, { headers });
    expect(((await perf.json()) as { sample_count: number }[])[0]?.sample_count).toBe(1);
  } finally {
    await request.delete(`/v0/management/providers/${provider}`, { headers });
    execFileSync('docker', ['compose', 'exec', '-T', 'postgres', 'psql', '-U', 'trefoil', '-d', 'trefoil', '-v', 'ON_ERROR_STOP=1', '-q', '-c', `DELETE FROM provider_performance WHERE provider='${provider}'; DELETE FROM request_usage WHERE provider='${provider}';`], { cwd: resolve(import.meta.dirname, '../..'), stdio: 'pipe', timeout: 15_000 });
    server.closeAllConnections();
    await new Promise<void>(done => server.close(() => done()));
  }
});

test('clean EOF without SSE terminal marker is incomplete, not successful telemetry', async ({ request }) => {
  if (!admin) throw new Error('Local admin key required');
  const provider = `eof-telemetry-e2e-${randomUUID().replaceAll('-', '').slice(0, 8)}`;
  const headers = { 'x-admin-key': admin };
  const server = createServer((req, res) => {
    req.resume();
    req.on('end', () => {
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      res.write('data: {"choices":[{"delta":{"content":"partial"}}]}\n\n');
      setTimeout(() => res.end('data: {"usage":{"prompt_tokens":2,"completion_tokens":6}}\n\n'), 85);
    });
  });
  await new Promise<void>(done => server.listen(0, '0.0.0.0', done));
  try {
    const origin = `http://192.168.96.1:${(server.address() as { port: number }).port}`;
    expect((await request.put(`/v0/management/providers/${provider}`, { headers, data: { api_base_url: origin, api_key: 'disposable', enabled: true } })).status()).toBe(200);
    const response = await request.post('/v1/chat/completions', { headers: { 'x-api-key': admin }, data: { model: `${provider}/sample`, stream: true, messages: [{ role: 'user', content: 'hello' }] } });
    expect(response.status()).toBe(200);
    await response.body();
    await expect.poll(async () => {
      const usage = await request.get('/v0/management/usage?limit=20&offset=0', { headers });
      return ((await usage.json()).data as { provider: string; responseStatus: string }[]).find(row => row.provider === provider)?.responseStatus;
    }, { timeout: 5000 }).toBe('error');
    const usage = await request.get('/v0/management/usage?limit=20&offset=0', { headers });
    const row = ((await usage.json()).data as { provider: string; tokensOutput: number | null; ttftMs: number | null; tokensPerSec: number | null; e2eTokensPerSec: number | null }[]).find(row => row.provider === provider);
    expect(row).toMatchObject({ tokensOutput: 6, ttftMs: null, tokensPerSec: null, e2eTokensPerSec: null });
    const perf = await request.get(`/v0/management/performance?provider=${provider}`, { headers });
    expect((await perf.json())).toHaveLength(0);
    const empty = await request.get(`/v0/management/performance?provider=${provider}&model=never-seen`, { headers });
    expect(empty.status()).toBe(200);
    expect(await empty.json()).toEqual([]);
  } finally {
    await request.delete(`/v0/management/providers/${provider}`, { headers });
    execFileSync('docker', ['compose', 'exec', '-T', 'postgres', 'psql', '-U', 'trefoil', '-d', 'trefoil', '-v', 'ON_ERROR_STOP=1', '-q', '-c', `DELETE FROM provider_performance WHERE provider='${provider}'; DELETE FROM request_usage WHERE provider='${provider}';`], { cwd: resolve(import.meta.dirname, '../..'), stdio: 'pipe', timeout: 15_000 });
    server.closeAllConnections();
    await new Promise<void>(done => server.close(() => done()));
  }
});

test('empty usage summary distinguishes no observations from measured zero', async ({ request }) => {
  if (!admin) throw new Error('Local admin key required');
  const end = new Date(Date.now() - 365 * 24 * 60 * 60 * 1000);
  const start = new Date(end.getTime() - 60_000);
  const response = await request.get(`/v0/management/usage/summary?range=custom&startDate=${encodeURIComponent(start.toISOString())}&endDate=${encodeURIComponent(end.toISOString())}`, { headers: { 'x-admin-key': admin } });
  expect(response.status()).toBe(200);
  const data = await response.json() as { stats: { totalRequests: number; ttftSamples: number; tpsSamples: number; avgTtftMs: number; avgTokensPerSec: number }; series: unknown[] };
  expect(data.stats).toMatchObject({ totalRequests: 0, ttftSamples: 0, tpsSamples: 0 });
  expect(data.series).toHaveLength(0);
});
