import { describe, expect, test } from 'bun:test';
import { buildReport, parseResponseBody, parseSseResponse, recordCase, reconcileCaseCount, reconcileCaseInventory, registeredCaseNames, type CaseResult } from './differential';

const result = (name: string, status: CaseResult['status']): CaseResult => ({ name, status });

describe('differential report', () => {
  test('sorts cases and reports deterministic counts', () => {
    expect(buildReport([result('z', 'error'), result('a', 'pass'), result('m', 'fail')], 'complete', 3)).toEqual({
      schema_version: 1, status: 'complete', expected_cases: 3, observed_cases: 3,
      missing_cases: 0, duplicate_cases: 0, counts: { pass: 1, fail: 1, error: 1 },
      cases: [result('a', 'pass'), result('m', 'fail'), result('z', 'error')],
    });
  });

  test('reports missing cases for incomplete runs', () => {
    expect(buildReport([result('only', 'pass')], 'incomplete', 4).missing_cases).toBe(3);
  });
});

describe('differential reconciliation', () => {
  test('exports a unique complete registered inventory', () => {
    const names = registeredCaseNames();
    expect(names.length).toBe(407);
    expect(new Set(names).size).toBe(names.length);
  });

  test('parses JSON, text, SSE, and rejects malformed JSON', () => {
    expect(parseResponseBody('{"b":2}', 'application/json')).toEqual({ b: 2 });
    expect(parseResponseBody('plain text', 'text/plain')).toBe('plain text');
    expect(parseSseResponse('event: message\ndata: {"result":{"ok":true}}\n\n')).toEqual({ result: { ok: true } });
    expect(() => parseResponseBody('{', 'application/json')).toThrow('malformed JSON response');
  });

  test('rejects duplicate case names', () => {
    const results: CaseResult[] = [];
    recordCase(result('same', 'pass'), results);
    expect(() => recordCase(result('same', 'fail'), results)).toThrow('duplicate differential case name: same');
  });

  test('accepts exact inventory and rejects drift', () => {
    expect(() => reconcileCaseCount(3, 3)).not.toThrow();
    expect(() => reconcileCaseCount(2, 3)).toThrow('inventory reconciliation failed: expected 3, observed 2');
  });

  test('rejects replacement, missing, and duplicate case names', () => {
    expect(() => reconcileCaseInventory(['a', 'replacement'], ['a', 'b'])).toThrow(
      'case inventory reconciliation failed: missing: b; unexpected: replacement'
    );
    expect(() => reconcileCaseInventory(['a', 'a'], ['a', 'b'])).toThrow(
      'case inventory reconciliation failed: duplicate observed: a; missing: b'
    );
    expect(() => reconcileCaseInventory(['a', 'b'], ['a', 'a'])).toThrow(
      'case inventory reconciliation failed: duplicate registered: a; unexpected: b'
    );
    expect(() => reconcileCaseInventory(['a', 'b'], ['a', 'b'])).not.toThrow();
  });

  test('reports every inventory mismatch in deterministic order', () => {
    expect(() => reconcileCaseInventory(['z', 'z', 'x'], ['b', 'a', 'b'])).toThrow(
      'case inventory reconciliation failed: duplicate registered: b; duplicate observed: z; missing: a, b; unexpected: x, z'
    );
  });
});

describe('backup binary comparison', () => {
  test('rejects malformed and incomplete archives', async () => {
    const { parseBackupArchive } = await import('./differential');
    const { gzipSync } = await import('node:zlib');
    expect(() => parseBackupArchive(new Uint8Array([1, 2, 3]))).toThrow();
    expect(() => parseBackupArchive(gzipSync(Buffer.alloc(1024)))).toThrow('missing manifest/config');
  });
});

test('backup parser compares decompressed JSON and preserves table contents', async () => {
  const { parseBackupArchive } = await import('./differential');
  const { gzipSync } = await import('node:zlib');
  const entry = (name: string, text: string) => {
    const bytes = Buffer.from(text);
    const header = Buffer.alloc(512);
    header.write(name);
    header.write(bytes.length.toString(8).padStart(11, '0') + '\0', 124);
    header[156] = 48;
    return Buffer.concat([header, bytes, Buffer.alloc((512 - bytes.length % 512) % 512)]);
  };
  const archive = gzipSync(Buffer.concat([
    entry('manifest.json', JSON.stringify({ version: 1, created_at: 'today', dialect: 'sqlite' })),
    entry('config.json', JSON.stringify({ keys: { fixture: { secret: 'test-only' } } })),
    entry('request_usage.csv', 'request_id\nfixture'), Buffer.alloc(1024),
  ]));
  expect(parseBackupArchive(archive)).toEqual({
    'manifest.json': { version: 1 }, 'config.json': { keys: { fixture: { secret: 'test-only' } } },
    'request_usage.csv': 'request_id\nfixture',
  });
});

test('audit normalization validates nondeterminism without hiding identities or counts', async () => {
  const { normalizeForCase } = await import('./differential');
  const input = { data: [{ request_id: '11111111-1111-4111-8111-111111111111', created_at: '2026-09-18T00:00:00Z', start_time: 1789740000000, duration_ms: 1, source_ip: '127.0.0.1', tool_name: 'plexus_config', response_status: 200 }], total: 1 };
  const result = normalizeForCase('/v0/management/mcp-logs?limit=5', input) as typeof input;
  expect(result.total).toBe(1);
  expect(result.data[0].source_ip).toBe('127.0.0.1');
  expect(result.data[0].tool_name).toBe('plexus_config');
  expect(result.data[0].request_id).toBe('<uuid>');
  expect(() => normalizeForCase('/v0/management/mcp-logs', { ...input, data: [{ ...input.data[0], request_id: 'bad' }] })).toThrow('Invalid MCP audit UUID');
});
