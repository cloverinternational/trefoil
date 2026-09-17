import type { ModelAlias, ModelTarget, ModelTargetGroup } from './api';

export const getModelOptionKey = (provider: string | undefined, model: string | undefined) =>
  `${provider ?? ''}\u0000${model ?? ''}`;

export function dedupeModels<T extends { id: string; providerId?: string }>(models: T[]): T[] {
  const seen = new Set<string>();
  return models.filter(model => {
    const key = getModelOptionKey(model.providerId, model.id);
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

export function dedupeById<T extends { id: string }>(items: T[]): T[] {
  const seen = new Set<string>();
  return items.filter(item => !seen.has(item.id) && (seen.add(item.id), true));
}

export function dedupeAliasTargets(groups: ModelTargetGroup[]): ModelTargetGroup[] {
  const seen = new Set<string>();
  return groups.map(group => ({ ...group, targets: group.targets.filter(target => {
    const key = target.alias ? `alias\u0000${target.alias}` : getModelOptionKey(target.provider, target.model);
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  }) }));
}

export type { ModelAlias, ModelTarget };
