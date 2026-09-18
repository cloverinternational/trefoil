export function formatCost(cost: number, decimals = 4): string {
  if (cost === 0) return `$${cost.toFixed(decimals)}`;
  const threshold = 10 ** -decimals;
  if (cost > 0 && cost < threshold) return `$<${threshold.toFixed(decimals)}`;
  if (cost >= 0.01) return `$${cost.toLocaleString(undefined, { minimumFractionDigits: decimals, maximumFractionDigits: decimals })}`;
  return `$${cost.toFixed(decimals)}`;
}

export type ResetCountdown = { short: string; long: string; absolute: string; remainingMs: number };
export function formatResetCountdown(iso: string | null | undefined, nowMs = Date.now()): ResetCountdown | null {
  if (!iso) return null;
  const date = new Date(iso); const remainingMs = date.getTime() - nowMs;
  if (!Number.isFinite(date.getTime())) return null;
  const absolute = date.toLocaleString();
  if (remainingMs <= 0) return { short: 'now', long: 'resetting now', absolute, remainingMs };
  const seconds = Math.floor(remainingMs / 1000); const days = Math.floor(seconds / 86400);
  const hours = Math.floor((seconds % 86400) / 3600); const minutes = Math.floor((seconds % 3600) / 60);
  const short = days > 7 ? date.toLocaleDateString(undefined, { month: 'short', day: 'numeric' }) : days ? `${days}d ${hours}h` : hours ? `${hours}h ${minutes}m` : minutes ? `${minutes}m` : `${seconds % 60}s`;
  return { short, long: days > 7 ? `resets ${short}` : `resets in ${short}`, absolute, remainingMs };
}
export function countdownTickMs(remainingMs: number): number { return remainingMs > 0 && remainingMs < 60_000 ? 1_000 : 30_000; }
export type FormatCostInOptions = { currency: string; rate: number; symbol?: string; decimals?: number };
export function formatCostIn(costUsd: number, options: FormatCostInOptions): string {
  const decimals = options.decimals ?? 4; const cost = costUsd * options.rate;
  const formatter = new Intl.NumberFormat(undefined, { style: 'currency', currency: options.currency, currencyDisplay: 'symbol', minimumFractionDigits: decimals, maximumFractionDigits: decimals });
  const format = (value: number, marker?: string) => { let integer = false; return formatter.formatToParts(value).map(part => { if (part.type === 'currency' && options.symbol) return options.symbol; if (marker && !integer && part.type === 'integer') { integer = true; return marker + part.value; } return part.value; }).join(''); };
  if (cost === 0) return format(cost); const threshold = 10 ** -decimals;
  return cost > 0 && cost < threshold ? format(threshold, '<') : format(cost);
}

export function getEstimatedBytesPerToken(options: { incomingApiType?: string | null; outgoingApiType?: string | null; isStreamed?: boolean | null }): number {
  if (options.isStreamed === false) return 4.5;
  const type = (options.incomingApiType || options.outgoingApiType || '').toLowerCase();
  if (type.includes('anthropic') || type.includes('messages') || type === 'oauth') return 140;
  if (type.includes('responses')) return 200;
  if (type.includes('openai') || type.includes('chat') || type.includes('antigravity')) return 215;
  if (type.includes('gemini') || type.includes('google')) return 140;
  return 140;
}
