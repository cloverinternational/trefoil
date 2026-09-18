import type { ModelAlias, ModelTarget } from './api';

export type ModelCatalog = Record<string, ModelAlias>;

function targetsFor(alias: ModelAlias): ModelTarget[] {
  return (alias.target_groups ?? []).flatMap(group => group.targets);
}

export function modelSearchText(slug: string, alias: ModelAlias): string {
  return [slug, alias.type, ...(alias.additional_aliases ?? []), ...targetsFor(alias).flatMap(target => [target.provider, target.model, target.alias])]
    .filter(Boolean).join(' ').toLowerCase();
}

export function filterAndSortModels(catalog: ModelCatalog, query: string): Array<[string, ModelAlias]> {
  const normalizedQuery = query.trim().toLowerCase();
  return Object.entries(catalog)
    .filter(([slug, alias]) => !normalizedQuery || modelSearchText(slug, alias).includes(normalizedQuery))
    .sort(([left], [right]) => left.localeCompare(right));
}

export function visibleModelTargets(alias: ModelAlias): ModelTarget[] {
  return targetsFor(alias).filter(target => target.provider || target.model || target.alias);
}
