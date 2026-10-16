import React, { useCallback, useEffect, useState } from 'react';
import { Boxes, Pencil, Plus, RefreshCw, Trash2 } from 'lucide-react';
import { Link } from 'react-router-dom';
import { toast } from 'sonner';
import { Button } from '../components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '../components/ui/card';
import { Skeleton } from '../components/ui/skeleton';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '../components/ui/table';
import { deleteAlias, getAliases, type ModelAlias } from '../lib/api';
import { modelSearchText, visibleModelTargets } from '../lib/modelList';
import { CatalogOrganizer } from '../components/CatalogOrganizer';

export const Models: React.FC = () => {
  const [aliases, setAliases] = useState<Record<string, ModelAlias>>({});
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const load = useCallback(async () => { setLoading(true); setError(null); try { setAliases(await getAliases()); } catch (cause) { setError(cause instanceof Error ? cause.message : 'Unable to load models'); } finally { setLoading(false); } }, []);
  useEffect(() => { void load(); }, [load]);
  const ids = Object.keys(aliases);
  const remove = async (slug: string) => { if (!window.confirm(`Delete model “${slug}”?`)) return; try { await deleteAlias(slug); await load(); toast.success('Model deleted'); } catch (cause) { const message = cause instanceof Error ? cause.message : 'Unable to delete model'; setError(message); toast.error(message); } };
  return <div className="mx-auto max-w-6xl space-y-6 p-6 sm:p-8">
    <header className="flex flex-wrap items-center justify-between gap-4"><div><h1 className="font-heading text-3xl font-semibold">Models</h1><p className="mt-1 text-sm text-muted-foreground">Browse and manage the model catalog exposed by this gateway.</p></div><div className="flex gap-2"><Button variant="outline" onClick={() => void load()} disabled={loading}><RefreshCw className={loading ? 'animate-spin' : ''} /> Refresh</Button><Button asChild><Link to="/aliases"><Plus /> Add model</Link></Button></div></header>
    {error && <p role="alert" className="rounded-md border border-destructive/40 bg-destructive/10 p-3 text-sm text-destructive">{error}</p>}
    {loading ? <Card role="status"><CardContent className="space-y-3 p-6">{[0, 1, 2, 3].map(i => <Skeleton key={i} className="h-12 w-full" />)}</CardContent></Card> : ids.length === 0 ? <Card><CardContent className="py-16 text-center"><Boxes className="mx-auto text-muted-foreground" /><p className="mt-3 font-medium">No models configured</p><p className="mt-1 text-sm text-muted-foreground">Add a model alias to expose a stable model name.</p></CardContent></Card> : <CatalogOrganizer kind="model" ids={ids} searchText={id => modelSearchText(id, aliases[id])}>{(visibleIds, _organization, controls) => {
      const entries = visibleIds.map(id => [id, aliases[id]] as [string, ModelAlias]);
      return <Card><CardHeader><CardTitle>Model catalog</CardTitle><CardDescription>Showing {entries.length} of {ids.length} models</CardDescription></CardHeader><CardContent className="overflow-x-auto"><Table className="min-w-[720px]"><TableHeader><TableRow><TableHead>Model</TableHead><TableHead>Provider targets</TableHead><TableHead>Organization</TableHead><TableHead className="text-right">Actions</TableHead></TableRow></TableHeader><TableBody>{entries.map(([slug, alias]) => { const targets = visibleModelTargets(alias); return <TableRow key={slug}><TableCell><div className="font-mono font-medium">{slug}</div>{alias.additional_aliases?.length ? <div className="mt-1 text-xs text-muted-foreground">Also known as {alias.additional_aliases.join(', ')}</div> : null}</TableCell><TableCell>{targets.length ? <ul className="space-y-1 text-xs text-muted-foreground">{targets.slice(0, 3).map((target, index) => <li key={`${slug}-${index}`} className="font-mono">{target.provider ? `${target.provider} / ` : ''}{target.model || target.alias || 'Unnamed target'}</li>)}{targets.length > 3 && <li>+{targets.length - 3} more targets</li>}</ul> : <span className="text-muted-foreground">No targets configured</span>}</TableCell><TableCell>{controls(slug)}</TableCell><TableCell><div className="flex justify-end gap-2"><Button variant="ghost" size="sm" asChild><Link to="/aliases"><Pencil /> Edit</Link></Button><Button variant="destructive" size="sm" onClick={() => void remove(slug)}><Trash2 /> Delete</Button></div></TableCell></TableRow>; })}</TableBody></Table></CardContent></Card>;
    }}</CatalogOrganizer>}
  </div>;
};
