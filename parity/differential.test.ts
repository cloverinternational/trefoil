import { describe, expect, test } from 'bun:test';
import { buildReport, recordCase, reconcileCaseCount, reconcileCaseInventory, type CaseResult } from './differential';

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
