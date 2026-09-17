export function formatCost(cost: number, decimals = 4): string {
  if (cost === 0) return `$${cost.toFixed(decimals)}`;
  const threshold = 10 ** -decimals;
  if (cost > 0 && cost < threshold) return `$<${threshold.toFixed(decimals)}`;
  if (cost >= 0.01) return `$${cost.toLocaleString(undefined, { minimumFractionDigits: decimals, maximumFractionDigits: decimals })}`;
  return `$${cost.toFixed(decimals)}`;
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
