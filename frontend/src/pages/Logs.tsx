import React, { useCallback, useEffect, useState } from 'react';
import { AlertTriangle, ArrowLeft, ChevronLeft, ChevronRight, FileText, RefreshCw, Trash2 } from 'lucide-react';
import { Link } from 'react-router-dom';
import { useAuth } from '../contexts/AuthContext';
import { deleteUsage, getUsage, type UsageRecord } from '../lib/api';

const formatTime = (value: number, fallback: string) =>
  new Date(value < 10_000_000_000 ? value * 1000 : value || fallback).toLocaleString();
const formatNumber = (value: number | null) => value == null ? '—' : new Intl.NumberFormat().format(value);

export const Logs: React.FC = () => {
  const { isAdmin } = useAuth();
  const [rows, setRows] = useState<UsageRecord[]>([]);
  const [total, setTotal] = useState(0);
  const [offset, setOffset] = useState(0);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const limit = 25;

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const result = await getUsage(limit, offset);
      setRows(result.data);
      setTotal(result.total);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : 'Unable to load request logs');
    } finally {
      setLoading(false);
    }
  }, [offset]);

  useEffect(() => { void load(); }, [load]);

  const remove = async (requestId: string) => {
    if (!window.confirm('Delete this request log?')) return;
    try { await deleteUsage(requestId); await load(); }
    catch (cause) { setError(cause instanceof Error ? cause.message : 'Unable to delete request log'); }
  };

  return <div className="min-h-screen bg-bg-deep text-text">
    <header className="border-b border-border bg-slate-950/70 px-4 py-4 sm:px-8">
      <div className="mx-auto flex max-w-6xl items-center justify-between gap-4">
        <div><p className="text-xs font-medium uppercase tracking-[0.2em] text-primary">Trefoil control plane</p>
          <h1 className="mt-1 flex items-center gap-2 font-heading text-2xl font-semibold"><FileText size={22} className="text-primary" /> Logs</h1>
          <p className="mt-1 text-sm text-text-secondary">Review gateway requests, outcomes, and latency.</p></div>
        <button onClick={() => void load()} disabled={loading} aria-label="Refresh logs" className="inline-flex items-center gap-2 rounded-md border border-border-2 px-3 py-2 text-sm text-text-secondary hover:bg-slate-800 disabled:opacity-50"><RefreshCw size={15} className={loading ? 'animate-spin' : ''} /><span className="hidden sm:inline">Refresh</span></button>
      </div>
    </header>
    <main className="mx-auto max-w-6xl space-y-5 px-4 py-6 sm:px-8 sm:py-8">
      <Link to="/" className="inline-flex items-center gap-2 text-sm text-text-secondary hover:text-text"><ArrowLeft size={15} /> Back to overview</Link>
      {error && <div role="alert" className="flex items-center gap-3 rounded-lg border border-red-500/30 bg-red-500/10 p-3 text-sm text-red-200"><AlertTriangle size={17} />{error}<button className="ml-auto underline" onClick={() => void load()}>Retry</button></div>}
      <section className="glass-bg overflow-hidden rounded-xl"><div className="flex items-center gap-2 border-b border-border p-4"><FileText size={18} className="text-primary" /><h2 className="font-heading text-lg font-semibold">Request log</h2><span className="ml-auto text-xs text-text-muted">{total ? `${offset + 1}–${Math.min(offset + limit, total)} of ${total}` : 'No records'}</span></div>
        {loading ? <p className="py-16 text-center text-sm text-text-muted">Loading request logs…</p> : rows.length === 0 ? <div className="py-16 text-center"><FileText size={26} className="mx-auto text-text-muted" /><p className="mt-3 font-medium">No request logs</p><p className="mt-1 text-sm text-text-secondary">Requests will appear here after the gateway handles traffic.</p></div> : <div className="overflow-x-auto"><table className="w-full min-w-[760px] text-left text-sm"><thead className="bg-slate-900/50 text-xs uppercase tracking-wider text-text-muted"><tr><th className="px-5 py-3 font-medium">Request</th><th className="px-5 py-3 font-medium">Model / provider</th><th className="px-5 py-3 font-medium">Tokens</th><th className="px-5 py-3 font-medium">Status</th>{isAdmin && <th className="px-5 py-3" />}</tr></thead><tbody className="divide-y divide-border">{rows.map(row => <tr key={row.requestId} className="hover:bg-slate-900/30"><td className="px-5 py-3"><p>{formatTime(row.startTime, row.date)}</p><p className="mt-1 max-w-[190px] truncate font-mono text-xs text-text-muted" title={row.requestId}>{row.requestId}</p></td><td className="px-5 py-3"><p className="font-medium">{row.incomingModelAlias || '—'}</p><p className="mt-1 text-xs text-text-muted">{row.provider || 'Provider unavailable'}</p></td><td className="px-5 py-3 text-text-secondary">{formatNumber(row.tokensInput)} in · {formatNumber(row.tokensOutput)} out</td><td className="px-5 py-3"><span className={row.responseStatus === 'success' ? 'text-emerald-300' : 'text-red-300'}>{row.responseStatus || 'Unknown'}</span>{row.durationMs != null && <p className="mt-1 text-xs text-text-muted">{row.durationMs}ms</p>}</td>{isAdmin && <td className="px-5 py-3 text-right"><button aria-label={`Delete request ${row.requestId}`} onClick={() => void remove(row.requestId)} className="rounded p-1.5 text-text-muted hover:bg-red-500/10 hover:text-red-300"><Trash2 size={15} /></button></td>}</tr>)}</tbody></table></div>}
        <div className="flex justify-end gap-2 border-t border-border p-3"><button disabled={!offset || loading} onClick={() => setOffset(Math.max(0, offset - limit))} aria-label="Previous page" className="rounded-md border border-border-2 p-2 disabled:opacity-40"><ChevronLeft size={16} /></button><button disabled={offset + limit >= total || loading} onClick={() => setOffset(offset + limit)} aria-label="Next page" className="rounded-md border border-border-2 p-2 disabled:opacity-40"><ChevronRight size={16} /></button></div>
      </section>
    </main>
  </div>;
};
