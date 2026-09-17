import { describe, expect, it } from 'vitest';
import { dedupeAliasTargets, dedupeById, dedupeModels, getModelOptionKey } from './modelOptions';

describe('model option parity helpers', () => {
  it('deduplicates models by provider and id, not id alone', () => {
    const models = [{ id: 'same', providerId: 'one' }, { id: 'same', providerId: 'one' }, { id: 'same', providerId: 'two' }];
    expect(dedupeModels(models)).toEqual([models[0], models[2]]);
    expect(getModelOptionKey('one', 'same')).not.toBe(getModelOptionKey('two', 'same'));
  });
  it('deduplicates catalog ids and alias targets across groups', () => {
    expect(dedupeById([{ id: 'a' }, { id: 'a' }, { id: 'b' }])).toEqual([{ id: 'a' }, { id: 'b' }]);
    const groups = [{ name: 'a', selector: 'in_order', targets: [{ provider: 'p', model: 'm' }] }, { name: 'b', selector: 'in_order', targets: [{ provider: 'p', model: 'm' }, { alias: 'fallback' }] }];
    expect(dedupeAliasTargets(groups)[1].targets).toEqual([{ alias: 'fallback' }]);
  });
});
