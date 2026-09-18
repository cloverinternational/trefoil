import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { CURRENCY_RATE_CACHE_MAX_AGE_MS, CURRENCY_RATES_STORAGE_KEY, CURRENCY_RATES_URL, DEFAULT_CURRENCY, DISPLAY_CURRENCY_STORAGE_KEY, SUPPORTED_CURRENCIES, fetchRates, getCachedRates, getCurrency, getSelectedCurrency, isCurrencyRateCacheFresh, setSelectedCurrency } from './currency';

const values = new Map<string, string>();
const testStorage = { get length() { return values.size; }, clear: () => values.clear(), getItem: (key: string) => values.get(key) ?? null, key: (index: number) => [...values.keys()][index] ?? null, removeItem: (key: string) => values.delete(key), setItem: (key: string, value: string) => values.set(key, String(value)) } as Storage;
beforeEach(() => { values.clear(); vi.stubGlobal('localStorage', testStorage); });
afterEach(() => vi.unstubAllGlobals());

describe('currency helpers', () => {
  it('fetches, normalizes, and caches rates', async () => {
    vi.spyOn(Date, 'now').mockReturnValue(1_700_000_000_000);
    const fetchSpy = vi.spyOn(globalThis, 'fetch').mockResolvedValue({ ok: true, json: async () => ({ date: '2026-08-01', rates: { EUR: 0.9, GBP: 0.8 } }) } as Response);
    await expect(fetchRates()).resolves.toEqual({ base: DEFAULT_CURRENCY, date: '2026-08-01', rates: { EUR: 0.9, GBP: 0.8, USD: 1 } });
    expect(fetchSpy).toHaveBeenCalledWith(CURRENCY_RATES_URL); expect(getCachedRates()?.fetchedAt).toBe(1_700_000_000_000);
  });
  it('uses fresh cache, handles stale fallback, and filters invalid rates', async () => {
    const now = 1_700_000_000_000; vi.spyOn(Date, 'now').mockReturnValue(now);
    testStorage.setItem(CURRENCY_RATES_STORAGE_KEY, JSON.stringify({ date: '2026-07-31', fetchedAt: now - CURRENCY_RATE_CACHE_MAX_AGE_MS, rates: { EUR: 0.9, bad: 'x' } }));
    expect(getCachedRates()?.rates).toEqual({ EUR: 0.9, USD: 1 }); expect(isCurrencyRateCacheFresh(getCachedRates()!, now + 1)).toBe(false);
    vi.spyOn(globalThis, 'fetch').mockRejectedValue(new Error('offline')); await expect(fetchRates()).resolves.toMatchObject({ date: '2026-07-31' });
  });
  it('returns null without cache when fetching fails', async () => { vi.spyOn(globalThis, 'fetch').mockRejectedValue(new Error('offline')); await expect(fetchRates()).resolves.toBeNull(); });
  it('rejects malformed payloads and preserves a valid stale cache', async () => {
    const now = 1_700_000_000_000; vi.spyOn(Date, 'now').mockReturnValue(now);
    testStorage.setItem(CURRENCY_RATES_STORAGE_KEY, JSON.stringify({ date: '2026-07-31', fetchedAt: now - CURRENCY_RATE_CACHE_MAX_AGE_MS - 1, rates: { EUR: 0.9 } }));
    vi.spyOn(globalThis, 'fetch').mockResolvedValue({ ok: true, json: async () => ({ rates: {} }) } as Response);
    await expect(fetchRates()).resolves.toEqual({ base: DEFAULT_CURRENCY, date: '2026-07-31', rates: { EUR: 0.9, USD: 1 } });
  });
  it('ignores storage failures and falls back to USD', () => {
    vi.stubGlobal('localStorage', { getItem: () => { throw new Error('blocked'); }, setItem: () => { throw new Error('blocked'); } });
    expect(getSelectedCurrency()).toBe(DEFAULT_CURRENCY); expect(() => setSelectedCurrency('EUR')).not.toThrow();
  });
  it('persists supported selections and falls back for unknown values', () => {
    expect(getSelectedCurrency()).toBe(DEFAULT_CURRENCY); setSelectedCurrency('EUR'); expect(testStorage.getItem(DISPLAY_CURRENCY_STORAGE_KEY)).toBe('EUR'); expect(getSelectedCurrency()).toBe('EUR');
    testStorage.setItem(DISPLAY_CURRENCY_STORAGE_KEY, 'nope'); expect(getSelectedCurrency()).toBe(DEFAULT_CURRENCY); expect(getCurrency('EUR')).toEqual({ code: 'EUR', symbol: '€', label: 'Euro' }); expect(getCurrency('nope')).toEqual(SUPPORTED_CURRENCIES[0]);
  });
});
