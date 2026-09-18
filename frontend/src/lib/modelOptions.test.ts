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
  it('keeps distinct models when providers differ or fields are absent', () => {
    const models = [
      { id: 'm', providerId: undefined },
      { id: 'm', providerId: 'openai' },
      { id: 'm', providerId: undefined },
    ];
    expect(dedupeModels(models)).toEqual([models[0], models[1]]);
    expect(getModelOptionKey(undefined, undefined)).toBe('\u0000');
    expect(getModelOptionKey('p', undefined)).not.toBe(getModelOptionKey(undefined, 'p'));
  });
  it('uses aliases as global keys and does not mutate input groups', () => {
    const groups = [
      { name: 'first', selector: 'fallback', targets: [{ alias: 'shared' }, { provider: 'p', model: 'm' }] },
      { name: 'second', selector: 'fallback', targets: [{ alias: 'shared' }, { provider: 'p', model: 'm' }, { provider: 'p', model: 'n' }] },
    ];
    const result = dedupeAliasTargets(groups);
    expect(result).toEqual([
      { ...groups[0], targets: groups[0].targets },
      { ...groups[1], targets: [{ provider: 'p', model: 'n' }] },
    ]);
    expect(groups[1].targets).toHaveLength(3);
  });
});
