export const CURRENCY_RATES_STORAGE_KEY = 'plexus.currencyRates';
export const DISPLAY_CURRENCY_STORAGE_KEY = 'plexus.displayCurrency';
export const CURRENCY_RATE_CACHE_MAX_AGE_MS = 12 * 60 * 60 * 1000;
export const DEFAULT_CURRENCY = 'USD';
export const CURRENCY_RATES_URL = 'https://api.frankfurter.dev/v1/latest?base=USD';

export type Currency = { code: string; symbol: string; label: string };
export const SUPPORTED_CURRENCIES = [
  { code: 'USD', symbol: '$', label: 'US Dollar' }, { code: 'EUR', symbol: '€', label: 'Euro' },
  { code: 'GBP', symbol: '£', label: 'British Pound' }, { code: 'JPY', symbol: '¥', label: 'Japanese Yen' },
  { code: 'AUD', symbol: 'A$', label: 'Australian Dollar' }, { code: 'CAD', symbol: 'C$', label: 'Canadian Dollar' },
  { code: 'CHF', symbol: 'CHF', label: 'Swiss Franc' }, { code: 'CNY', symbol: '¥', label: 'Chinese Yuan' },
  { code: 'HKD', symbol: 'HK$', label: 'Hong Kong Dollar' }, { code: 'NZD', symbol: 'NZ$', label: 'New Zealand Dollar' },
  { code: 'SEK', symbol: 'kr', label: 'Swedish Krona' }, { code: 'KRW', symbol: '₩', label: 'South Korean Won' },
  { code: 'SGD', symbol: 'S$', label: 'Singapore Dollar' }, { code: 'NOK', symbol: 'kr', label: 'Norwegian Krone' },
  { code: 'MXN', symbol: 'MX$', label: 'Mexican Peso' }, { code: 'INR', symbol: '₹', label: 'Indian Rupee' },
  { code: 'BRL', symbol: 'R$', label: 'Brazilian Real' }, { code: 'ZAR', symbol: 'R', label: 'South African Rand' },
  { code: 'PLN', symbol: 'zł', label: 'Polish Zloty' }, { code: 'DKK', symbol: 'kr', label: 'Danish Krone' },
  { code: 'CZK', symbol: 'Kč', label: 'Czech Koruna' }, { code: 'HUF', symbol: 'Ft', label: 'Hungarian Forint' },
  { code: 'ILS', symbol: '₪', label: 'Israeli New Shekel' }, { code: 'PHP', symbol: '₱', label: 'Philippine Peso' },
  { code: 'MYR', symbol: 'RM', label: 'Malaysian Ringgit' }, { code: 'THB', symbol: '฿', label: 'Thai Baht' },
  { code: 'IDR', symbol: 'Rp', label: 'Indonesian Rupiah' }, { code: 'TRY', symbol: '₺', label: 'Turkish Lira' },
  { code: 'ISK', symbol: 'kr', label: 'Icelandic Krona' }, { code: 'RON', symbol: 'lei', label: 'Romanian Leu' },
  { code: 'BGN', symbol: 'лв', label: 'Bulgarian Lev' },
] as const satisfies readonly Currency[];
export type CurrencyCode = (typeof SUPPORTED_CURRENCIES)[number]['code'];
export type CurrencyRates = { base: 'USD'; date: string; rates: Record<string, number> };
export type CurrencyRateCache = { date: string; fetchedAt: number; rates: Record<string, number> };

const storage = (): Storage | null => typeof localStorage === 'undefined' ? null : localStorage;
function normalizeRates(value: unknown): Record<string, number> | null {
  if (!value || typeof value !== 'object') return null;
  const rates: Record<string, number> = {};
  for (const [code, rate] of Object.entries(value)) if (typeof rate === 'number' && Number.isFinite(rate)) rates[code] = rate;
  if (!Object.keys(rates).length) return null;
  rates[DEFAULT_CURRENCY] = 1;
  return rates;
}
export function getCachedRates(): CurrencyRateCache | null {
  try {
    const raw = storage()?.getItem(CURRENCY_RATES_STORAGE_KEY); if (!raw) return null;
    const parsed = JSON.parse(raw) as Record<string, unknown>;
    if (!parsed || typeof parsed !== 'object' || typeof parsed.date !== 'string' || typeof parsed.fetchedAt !== 'number' || !Number.isFinite(parsed.fetchedAt)) return null;
    const rates = normalizeRates(parsed.rates); return rates ? { date: parsed.date, fetchedAt: parsed.fetchedAt, rates } : null;
  } catch { return null; }
}
export function isCurrencyRateCacheFresh(cache: CurrencyRateCache, now = Date.now()): boolean {
  return now - cache.fetchedAt <= CURRENCY_RATE_CACHE_MAX_AGE_MS;
}
export async function fetchRates(): Promise<CurrencyRates | null> {
  const cached = getCachedRates();
  if (cached && isCurrencyRateCacheFresh(cached)) return { base: DEFAULT_CURRENCY, date: cached.date, rates: cached.rates };
  try {
    const response = await fetch(CURRENCY_RATES_URL); if (!response.ok) throw new Error('rate request failed');
    const data = await response.json() as Record<string, unknown>;
    if (!data || typeof data !== 'object' || typeof data.date !== 'string') throw new Error('invalid rates');
    const rates = normalizeRates(data.rates); if (!rates) throw new Error('invalid rates');
    const result = { base: DEFAULT_CURRENCY as 'USD', date: data.date, rates };
    try { storage()?.setItem(CURRENCY_RATES_STORAGE_KEY, JSON.stringify({ date: result.date, fetchedAt: Date.now(), rates })); } catch { /* optional cache */ }
    return result;
  } catch { return cached ? { base: DEFAULT_CURRENCY, date: cached.date, rates: cached.rates } : null; }
}
export function getCurrency(code: string): Currency { return SUPPORTED_CURRENCIES.find(c => c.code === code) ?? SUPPORTED_CURRENCIES[0]; }
export function getSelectedCurrency(): CurrencyCode {
  try { const value = storage()?.getItem(DISPLAY_CURRENCY_STORAGE_KEY); if (value && SUPPORTED_CURRENCIES.some(c => c.code === value)) return value as CurrencyCode; } catch { /* default */ }
  return DEFAULT_CURRENCY;
}
export function setSelectedCurrency(currency: CurrencyCode): void { try { storage()?.setItem(DISPLAY_CURRENCY_STORAGE_KEY, currency); } catch { /* optional persistence */ } }
