import { describe, expect, it } from 'vitest';
import { parseCustomDateRange, parsePresetFromQuery } from '../helpers';

describe('detailed usage helpers', () => {
  it('filters unknown metrics while preserving valid metrics', () => expect(parsePresetFromQuery('range=live&metrics=requests,unknown,velocity,errors').selectedMetrics).toEqual(['requests', 'velocity', 'errors']));
  it('falls back to defaults when no metrics are valid', () => expect(parsePresetFromQuery('metrics=unknown').selectedMetrics).toEqual(['requests', 'tokens', 'cost']));
  it('rejects incomplete and invalid custom dates', () => { expect(parseCustomDateRange('2026-09-01T00:00:00.000Z')).toBeNull(); expect(parseCustomDateRange('not-a-date', '2026-09-02T00:00:00.000Z')).toBeNull(); });
});
