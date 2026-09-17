import { describe, expect, it } from 'vitest';
import { formatCost, getEstimatedBytesPerToken } from './format';

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
});
