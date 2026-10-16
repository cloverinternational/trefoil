import React, { useEffect, useMemo, useState } from 'react';
import { Plus, Tag, Trash2 } from 'lucide-react';
import { Button } from './ui/button';
import { Input } from './ui/input';
import { Badge } from './ui/badge';
import { createCatalogGroup, deleteCatalogGroup, getCatalogOrganization, saveCatalogEntry, type CatalogEntry, type CatalogKind, type CatalogOrganization } from '../lib/catalogOrganization';

const nameRule = /^[a-z0-9]+(?:-[a-z0-9]+)*$/;
const empty: CatalogOrganization = { groups: [], items: { provider: {}, model: {}, alias: {}, key: {} } };
type Props = { kind: CatalogKind; ids: string[]; searchText: (id: string) => string; children: (visible: string[], organization: CatalogOrganization, controls: (id: string) => React.ReactNode) => React.ReactNode };

export const CatalogOrganizer: React.FC<Props> = ({ kind, ids, searchText, children }) => {
  const [data, setData] = useState<CatalogOrganization>(empty);
  const [search, setSearch] = useState('');
  const [group, setGroup] = useState('');
  const [tag, setTag] = useState('');
  const [draftGroup, setDraftGroup] = useState('');
  const [editing, setEditing] = useState<string | null>(null);
  const [draftTags, setDraftTags] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const reload = async () => { try { setData(await getCatalogOrganization()); setError(null); } catch (cause) { setError(cause instanceof Error ? cause.message : 'Organization unavailable'); } };
  useEffect(() => { void reload(); }, []);
  const tags = useMemo(() => [...new Set(ids.flatMap(id => data.items[kind]?.[id]?.tags ?? []))].sort((a,b) => a.localeCompare(b)), [ids, data, kind]);
  const visible = ids.filter(id => {
    const entry = data.items[kind]?.[id];
    return (!search || searchText(id).toLowerCase().includes(search.trim().toLowerCase())) && (!group || (group === '__unassigned__' ? !entry?.group : entry?.group === group)) && (!tag || entry?.tags.includes(tag));
  }).sort((a,b) => a.localeCompare(b));
  const action = async (work: () => Promise<void>) => { setBusy(true); setError(null); try { await work(); await reload(); } catch (cause) { setError(cause instanceof Error ? cause.message : 'Unable to save organization'); } finally { setBusy(false); } };
  const controls = (id: string) => {
    const entry: CatalogEntry = data.items[kind]?.[id] ?? { group: null, tags: [] };
    return <div className="flex min-w-[190px] flex-col gap-1 text-xs">
      <select aria-label={`Group for ${id}`} value={entry.group ?? ''} disabled={busy} onChange={event => void action(() => saveCatalogEntry(kind, id, { ...entry, group: event.target.value || null }))} className="h-8 rounded border border-border bg-background px-2 text-foreground"><option value="">Unassigned</option>{data.groups.map(value => <option key={value} value={value}>{value}</option>)}</select>
      {editing === id ? <div className="flex gap-1"><Input aria-label={`Tags for ${id}`} value={draftTags} onChange={event => setDraftTags(event.target.value)} placeholder="tag, tag" className="h-8" /><Button size="sm" disabled={busy} onClick={() => { const values = [...new Set(draftTags.split(',').map(value => value.trim()).filter(Boolean))]; if (values.some(value => value.length > 64 || !nameRule.test(value)) || values.length > 12) { setError('Use up to 12 lowercase slug tags (letters, numbers and single hyphens).'); return; } void action(async () => { await saveCatalogEntry(kind, id, { ...entry, tags: values }); setEditing(null); }); }}>Save</Button></div> : <button type="button" className="flex flex-wrap items-center gap-1 text-left text-muted-foreground hover:text-foreground" onClick={() => { setEditing(id); setDraftTags(entry.tags.join(', ')); }} aria-label={`Edit tags for ${id}`}><Tag className="size-3" />{entry.tags.length ? entry.tags.map(value => <Badge variant="outline" key={value}>{value}</Badge>) : 'Add tags'}</button>}
    </div>;
  };
  return <div className="space-y-3">
    <div className="flex flex-wrap gap-2 rounded-lg border border-border bg-card p-3">
      <Input aria-label={`Search ${kind}s`} placeholder="Search name or target…" value={search} onChange={event => setSearch(event.target.value)} className="min-w-44 flex-1" />
      <select aria-label="Filter group" value={group} onChange={event => setGroup(event.target.value)} className="h-9 rounded border border-border bg-background px-2"><option value="">All groups</option><option value="__unassigned__">Unassigned</option>{data.groups.map(value => <option key={value}>{value}</option>)}</select>
      <select aria-label="Filter tag" value={tag} onChange={event => setTag(event.target.value)} className="h-9 rounded border border-border bg-background px-2"><option value="">All tags</option>{tags.map(value => <option key={value}>{value}</option>)}</select>
      <Button variant="outline" onClick={() => { setSearch(''); setGroup(''); setTag(''); }}>Clear filters</Button>
      <Button variant="ghost" onClick={() => void reload()} disabled={busy} aria-label="Refresh organization">Refresh</Button>
    </div>
    <div className="flex flex-wrap items-center gap-2"><Input aria-label="New group name" placeholder="New group (lowercase-slug)" value={draftGroup} onChange={event => setDraftGroup(event.target.value)} className="w-48" /><Button size="sm" disabled={busy || draftGroup.trim().length > 64 || !nameRule.test(draftGroup.trim())} onClick={() => void action(async () => { await createCatalogGroup(draftGroup.trim()); setDraftGroup(''); })}><Plus />Create group</Button>{group && group !== '__unassigned__' && <Button size="sm" variant="destructive" disabled={busy} onClick={() => { if (window.confirm(`Delete organizational group “${group}”? Items will become unassigned.`)) void action(async () => { await deleteCatalogGroup(group); setGroup(''); }); }}><Trash2 />Delete selected group</Button>}</div>
    {error && <p role="alert" className="rounded border border-destructive p-2 text-sm text-destructive">{error}</p>}
    <p className="text-xs text-muted-foreground">Showing {visible.length} of {ids.length} · Groups and tags organize lists only; they do not change routing or permissions.</p>
    {ids.length > 0 && !visible.length ? <p className="rounded border border-border p-8 text-center text-sm text-muted-foreground">No items match these filters. Clear filters to see all items.</p> : children(visible, data, controls)}
  </div>;
};
