import { describe, expect, it } from 'vitest';
import { toBoolean, toEpochMs, toIsoString } from './normalize';

describe('normalization parity helpers', () => {
  it('normalizes boolean flags and applies unknown fallbacks', () => {
    expect(toBoolean(true)).toBe(true); expect(toBoolean(0)).toBe(false);
    expect(toBoolean('yes')).toBe(true); expect(toBoolean('off')).toBe(false);
    expect(toBoolean('unknown', true)).toBe(true);
  });
  it('accepts all supported textual forms and handles nullish values', () => {
    for (const value of ['TRUE', ' 1 ', 't', 'Yes', 'y', 'ON']) expect(toBoolean(value)).toBe(true);
    for (const value of ['FALSE', ' 0 ', 'f', 'No', 'n', 'OFF']) expect(toBoolean(value)).toBe(false);
    expect(toBoolean(null)).toBe(false);
    expect(toBoolean(undefined, true)).toBe(true);
    expect(toBoolean(2)).toBe(true);
    expect(toBoolean(-1)).toBe(true);
    expect(toBoolean(Number.NaN, true)).toBe(true);
  });
  it('normalizes valid timestamps and rejects invalid values', () => {
    const iso = '2026-02-09T17:36:14.297Z'; const epoch = Date.parse(iso);
    expect(toEpochMs(iso)).toBe(epoch); expect(toEpochMs(new Date(iso))).toBe(epoch);
    expect(toIsoString(String(epoch))).toBe(iso); expect(toEpochMs('invalid')).toBeNull();
  });
  it('preserves finite epochs and rejects invalid dates', () => {
    expect(toEpochMs(0)).toBe(0);
    expect(toEpochMs(Number.POSITIVE_INFINITY)).toBeNull();
    expect(toEpochMs(new Date('invalid'))).toBeNull();
    expect(toEpochMs(null)).toBeNull();
    expect(toIsoString(null)).toBeNull();
    expect(toIsoString('invalid')).toBeNull();
  });
});
