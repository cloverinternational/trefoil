import { describe, expect, it } from 'vitest';
import { filterAndSortModels, modelSearchText, visibleModelTargets } from './modelList';

describe('model list parity helpers', () => {
  const catalog = {
    zebra: { type: 'chat', additional_aliases: ['latest'], target_groups: [{ name: 'main', selector: 'in_order', targets: [{ provider: 'openai', model: 'gpt-4' }, {}] }] },
    alpha: { target_groups: [{ name: 'fallback', selector: 'fallback', targets: [{ alias: 'backup' }] }] },
  };

  it('searches aliases and targets and sorts catalog entries deterministically', () => {
    expect(filterAndSortModels(catalog, ' GPT-4 ')).toEqual([['zebra', catalog.zebra]]);
    expect(filterAndSortModels(catalog, '')).toEqual([['alpha', catalog.alpha], ['zebra', catalog.zebra]]);
    expect(modelSearchText('zebra', catalog.zebra)).toContain('latest openai gpt-4');
  });

  it('returns empty results for unmatched queries and hides empty targets', () => {
    expect(filterAndSortModels(catalog, 'missing')).toEqual([]);
    expect(visibleModelTargets(catalog.zebra)).toEqual([{ provider: 'openai', model: 'gpt-4' }]);
    expect(visibleModelTargets({})).toEqual([]);
  });
});
