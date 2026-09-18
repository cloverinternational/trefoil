import { describe, expect, it } from 'vitest';
import { countdownTickMs, formatCost, formatCostIn, formatResetCountdown, getEstimatedBytesPerToken } from './format';

describe('format parity helpers', () => {
  it('formats zero, tiny, and ordinary costs', () => {
    expect(formatCost(0)).toBe('$0.0000'); expect(formatCost(0.00005)).toBe('$<0.0001');
    expect(formatCost(1.2)).toBe('$1.2000');
  });
  it('keeps protocol-specific streaming estimates', () => {
    expect(getEstimatedBytesPerToken({ incomingApiType: 'messages', isStreamed: true })).toBe(140);
    expect(getEstimatedBytesPerToken({ incomingApiType: 'chat', isStreamed: true })).toBe(215);
    expect(getEstimatedBytesPerToken({ incomingApiType: 'responses', isStreamed: true })).toBe(200);
    expect(getEstimatedBytesPerToken({ isStreamed: false })).toBe(4.5);
  });
  it('formats reset countdowns and update cadence', () => {
    const now = Date.parse('2026-08-06T12:00:00.000Z');
    expect(formatResetCountdown(new Date(now + 3 * 3600000 + 12 * 60000).toISOString(), now)?.short).toBe('3h 12m');
    expect(formatResetCountdown(new Date(now - 1).toISOString(), now)?.long).toBe('resetting now');
    expect(formatResetCountdown('invalid', now)).toBeNull();
    expect(countdownTickMs(41_000)).toBe(1_000); expect(countdownTickMs(60_000)).toBe(30_000);
  });
  it('formats costs after currency conversion', () => {
    expect(formatCostIn(2, { currency: 'USD', rate: 0.9 })).toBe('$1.8000');
    expect(formatCostIn(0.00005, { currency: 'USD', rate: 0.9 })).toBe('$<0.0001');
    expect(formatCostIn(1.5, { currency: 'EUR', rate: 1, symbol: '€' })).toBe('€1.5000');
  });
});
