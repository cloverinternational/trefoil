import React, { useCallback, useEffect, useState } from 'react';
import { ChevronLeft, ChevronRight, FileText, RefreshCw, Search, Trash2 } from 'lucide-react';
import { useAuth } from '../contexts/AuthContext';
import { deleteUsage, getUsage, type UsageRecord } from '../lib/api';
import { Button } from '../components/ui/button';
import { Badge } from '../components/ui/badge';
import { Card } from '../components/ui/card';
import { Input } from '../components/ui/input';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '../components/ui/select';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '../components/ui/table';
import { Skeleton } from '../components/ui/skeleton';
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from '../components/ui/dialog';

const formatTime = (value: number, fallback: string) => new Date(value < 10_000_000_000 ? value * 1000 : value || fallback).toLocaleString();
const formatNumber = (value: number | null) => value == null ? '—' : new Intl.NumberFormat().format(value);
const formatCost = (value: number | null) => value == null ? '—' : `$${value.toFixed(6)}`;

export const Logs: React.FC = () => {
  const { isAdmin } = useAuth();
  const [rows, setRows] = useState<UsageRecord[]>([]);
  const [total, setTotal] = useState(0);
  const [offset, setOffset] = useState(0);
  const [limit, setLimit] = useState(20);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [filters, setFilters] = useState({ key: '', model: '', provider: '', start: '', end: '' });
  const [applied, setApplied] = useState(filters);
  const [selected, setSelected] = useState<UsageRecord | null>(null);

  const load = useCallback(async () => {
    setLoading(true); setError(null);
    try {
      const result = await getUsage(limit, offset, {
        apiKey: applied.key.trim(), incomingModelAlias: applied.model.trim(), provider: applied.provider.trim(),
        startDate: applied.start, endDate: applied.end,
      });
      setRows(result.data); setTotal(result.total);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : 'Unable to load request logs');
    } finally { setLoading(false); }
  }, [limit, offset, applied]);
  useEffect(() => { void load(); }, [load]);

  const search = () => {
    setOffset(0);
    setApplied({ ...filters });
    if (offset === 0 && Object.keys(filters).every(key => filters[key as keyof typeof filters] === applied[key as keyof typeof applied])) void load();
  };
  const remove = async (requestId: string) => {
    if (!isAdmin || !window.confirm('Delete this request log?')) return;
    try { await deleteUsage(requestId); setSelected(null); await load(); }
    catch (cause) { setError(cause instanceof Error ? cause.message : 'Unable to delete request log'); }
  };

  return <main className="mx-auto w-full max-w-[1700px] space-y-5 p-4 sm:p-6">
    <header className="flex flex-wrap items-center justify-between gap-4 border-b border-border pb-4">
      <div><h1 className="flex items-center gap-2 text-2xl font-semibold"><FileText className="size-5 text-primary"/>Logs</h1><p className="mt-1 text-sm text-muted-foreground">All API requests routed through the gateway</p></div>
      <Button variant="outline" size="sm" onClick={() => void load()} disabled={loading}><RefreshCw className={loading ? 'animate-spin' : ''}/>Refresh</Button>
    </header>
    <section aria-label="Log filters" className="grid gap-2 rounded-lg border border-border bg-card p-3 md:grid-cols-3">
      <label className="relative"><span className="sr-only">Filter API key</span><Search className="absolute left-3 top-2.5 size-4 text-muted-foreground"/><Input className="pl-9" placeholder="Key…" value={filters.key} onChange={e => setFilters({ ...filters, key: e.target.value })}/></label>
      <label className="relative"><span className="sr-only">Filter model</span><Search className="absolute left-3 top-2.5 size-4 text-muted-foreground"/><Input className="pl-9" placeholder="Model…" value={filters.model} onChange={e => setFilters({ ...filters, model: e.target.value })}/></label>
      <label className="relative"><span className="sr-only">Filter provider</span><Search className="absolute left-3 top-2.5 size-4 text-muted-foreground"/><Input className="pl-9" placeholder="Provider…" value={filters.provider} onChange={e => setFilters({ ...filters, provider: e.target.value })}/></label>
      <label><span className="sr-only">Start date</span><Input type="date" value={filters.start} onChange={e => setFilters({ ...filters, start: e.target.value })}/></label>
      <label><span className="sr-only">End date</span><Input type="date" value={filters.end} onChange={e => setFilters({ ...filters, end: e.target.value })}/></label>
      <div className="flex items-end gap-2"><Button onClick={search}>Search</Button><span className="ml-auto text-xs text-muted-foreground">Per page</span><Select value={String(limit)} onValueChange={value => { setLimit(Number(value)); setOffset(0); }}><SelectTrigger className="w-24" aria-label="Rows per page"><SelectValue/></SelectTrigger><SelectContent>{[20, 50, 100, 200].map(size => <SelectItem key={size} value={String(size)}>{size}</SelectItem>)}</SelectContent></Select></div>
      <p className="text-xs text-muted-foreground md:col-span-3">Filters search all request logs before pagination. Limited keys can only view their own records.</p>
    </section>
    {error && <div role="alert" className="flex items-center gap-3 rounded-md border border-destructive/30 bg-destructive/10 p-3 text-sm text-destructive">{error}<Button variant="link" className="ml-auto" onClick={() => void load()}>Retry</Button></div>}
    <Card className="overflow-hidden">
      <div className="flex items-center justify-between border-b border-border px-4 py-3 text-xs text-muted-foreground"><span>{rows.length ? `${offset + 1}–${Math.min(offset + rows.length, total)} of ${total}` : `${total} total requests`}</span><span>Page {Math.floor(offset / limit) + 1} of {Math.max(1, Math.ceil(total / limit))}</span></div>
      {loading ? <div className="space-y-2 p-4">{Array.from({ length: 6 }, (_, i) => <Skeleton key={i} className="h-12 w-full"/>)}</div> : rows.length === 0 ? <div className="py-16 text-center text-sm text-muted-foreground">{total ? 'No requests on this page.' : 'No requests match these filters.'}</div> :
        <div className="overflow-x-auto"><Table className="min-w-[980px]"><TableHeader><TableRow><TableHead>Date</TableHead><TableHead>Key</TableHead><TableHead>API</TableHead><TableHead>Model</TableHead><TableHead>Tokens</TableHead><TableHead>Cost</TableHead><TableHead>Perf</TableHead><TableHead>Status</TableHead>{isAdmin && <TableHead className="w-12"/>}</TableRow></TableHeader><TableBody>{rows.map(row => <TableRow key={row.requestId} className="cursor-pointer" onClick={() => setSelected(row)}>
          <TableCell className="whitespace-nowrap"><span>{formatTime(row.startTime, row.date)}</span><span className="block font-mono text-[11px] text-muted-foreground">{row.requestId.slice(0, 12)}</span></TableCell>
          <TableCell className="max-w-36 truncate" title={row.apiKey ?? ''}>{row.apiKey || '—'}</TableCell><TableCell>{row.provider || '—'}</TableCell>
          <TableCell><span>{row.incomingModelAlias || '—'}</span><span className="block text-xs text-muted-foreground">{row.incomingModelAlias || 'Model unavailable'}</span></TableCell>
          <TableCell className="whitespace-nowrap">{formatNumber(row.tokensInput)} in · {formatNumber(row.tokensOutput)} out</TableCell><TableCell className="font-mono text-xs">{formatCost(row.costTotal)}</TableCell><TableCell className="whitespace-nowrap text-xs">{row.durationMs == null ? '—' : `Duration: ${(row.durationMs / 1000).toFixed(1)}s`}<span className="block">TTFT: {row.ttftMs == null ? '—' : `${formatNumber(row.ttftMs)} ms`}</span><span className="block">TPS: {row.tokensPerSec == null ? '—' : `${formatNumber(row.tokensPerSec)} tok/s`}</span><span className="block">E2E: {row.e2eTokensPerSec == null ? '—' : `${formatNumber(row.e2eTokensPerSec)} tok/s`}</span></TableCell>
          <TableCell><Badge variant={row.responseStatus === 'success' ? 'success' : row.responseStatus ? 'destructive' : 'secondary'}>{row.responseStatus || 'Unknown'}</Badge></TableCell>
          {isAdmin && <TableCell><Button variant="ghost" size="icon" aria-label={`Delete request ${row.requestId}`} onClick={e => { e.stopPropagation(); void remove(row.requestId); }}><Trash2/></Button></TableCell>}
        </TableRow>)}</TableBody></Table></div>}
      <div className="flex items-center justify-end gap-3 border-t border-border p-3"><span className="text-xs text-muted-foreground">Page {Math.floor(offset / limit) + 1} of {Math.max(1, Math.ceil(total / limit))}</span><Button variant="outline" size="icon" disabled={!offset || loading} onClick={() => setOffset(Math.max(0, offset - limit))} aria-label="Previous page"><ChevronLeft/></Button><Button variant="outline" size="icon" disabled={offset + limit >= total || loading} onClick={() => setOffset(offset + limit)} aria-label="Next page"><ChevronRight/></Button></div>
    </Card>
    <Dialog open={!!selected} onOpenChange={open => !open && setSelected(null)}><DialogContent className="max-h-[85vh] overflow-y-auto sm:max-w-2xl"><DialogHeader><DialogTitle>Request details</DialogTitle><DialogDescription className="font-mono">{selected?.requestId}</DialogDescription></DialogHeader>{selected && <pre className="overflow-auto rounded-md border border-border bg-muted/40 p-4 font-mono text-xs">{JSON.stringify(selected, null, 2)}</pre>}</DialogContent></Dialog>
  </main>;
};
