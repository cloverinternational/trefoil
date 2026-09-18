import { describe, expect, it } from 'vitest';
import { calcVelocity, createMetrics, formatBucketLabel, getRangeConfig, getSummaryBreakdown, parseCustomDateRange, parsePresetFromQuery } from '../helpers';
describe('detailed usage helpers', () => {
  it('filters unknown metrics while preserving valid metrics', () => expect(parsePresetFromQuery('range=live&metrics=requests,unknown,velocity,errors').selectedMetrics).toEqual(['requests', 'velocity', 'errors']));
  it('falls back to defaults when no metrics are valid', () => expect(parsePresetFromQuery('metrics=unknown').selectedMetrics).toEqual(['requests', 'tokens', 'cost']));
  it('rejects incomplete and invalid custom dates', () => { expect(parseCustomDateRange('2026-09-01T00:00:00.000Z')).toBeNull(); expect(parseCustomDateRange('not-a-date', '2026-09-02T00:00:00.000Z')).toBeNull(); });
  it('parses supported controls and ignores invalid values', () => expect(parsePresetFromQuery('?range=week&chartType=bar&groupBy=model&viewMode=list&metrics=requests,errors')).toMatchObject({ timeRange: 'week', chartType: 'bar', groupBy: 'model', viewMode: 'list', selectedMetrics: ['requests', 'errors'] }));
  it('creates currency-aware metrics and stable range/breakdown helpers', () => { const metrics = createMetrics({ currency: 'EUR', rate: 0.9, symbol: '€' }); expect(metrics.map(metric => metric.key)).toEqual(['requests', 'tokens', 'cost', 'duration', 'ttft', 'tps', 'errors']); expect(metrics.find(metric => metric.key === 'cost')?.label).toBe('Cost (€)'); expect(getRangeConfig('live').minutes).toBe(5); expect(getSummaryBreakdown('model', 'chart')).toEqual(['modelAlias']); expect(getSummaryBreakdown('time', 'chart')).toEqual([]); expect(formatBucketLabel('hour', Date.UTC(2026, 0, 1, 12))).toMatch(/(08|12)/); });
  it('calculates velocity from cumulative request points', () => expect(calcVelocity([{ requests: 2 }, { requests: 7 }, { requests: 6 }] as never).map(point => point.velocity)).toEqual([0, 5, -1]));
});
