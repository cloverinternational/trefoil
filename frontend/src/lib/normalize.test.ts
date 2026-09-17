import { describe, expect, it } from 'vitest';
import { toBoolean, toEpochMs, toIsoString } from './normalize';

describe('normalization parity helpers', () => {
  it('normalizes boolean flags and applies unknown fallbacks', () => {
    expect(toBoolean(true)).toBe(true); expect(toBoolean(0)).toBe(false);
    expect(toBoolean('yes')).toBe(true); expect(toBoolean('off')).toBe(false);
    expect(toBoolean('unknown', true)).toBe(true);
  });
  it('normalizes valid timestamps and rejects invalid values', () => {
    const iso = '2026-02-09T17:36:14.297Z'; const epoch = Date.parse(iso);
    expect(toEpochMs(iso)).toBe(epoch); expect(toEpochMs(new Date(iso))).toBe(epoch);
    expect(toIsoString(String(epoch))).toBe(iso); expect(toEpochMs('invalid')).toBeNull();
  });
});
