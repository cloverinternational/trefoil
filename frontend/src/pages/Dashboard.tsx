import React from 'react';
import { useCallback, useEffect, useState } from 'react';
import { Activity, AlertTriangle, BarChart3, Boxes, CircleDollarSign, Clock3, KeyRound, LogOut, RefreshCw, Server, ShieldCheck, TriangleAlert } from 'lucide-react';
import { Link } from 'react-router-dom';
import { useAuth } from '../contexts/AuthContext';
import { getConfigStatus, getRecentUsage, getUsageSummary, type UsageRecord } from '../lib/api';

const number = new Intl.NumberFormat();
const money = new Intl.NumberFormat(undefined, { style: 'currency', currency: 'USD', maximumFractionDigits: 4 });

function displayTime(value: number, date: string) {
  if (!value && date) return date;
  return new Date(value < 10_000_000_000 ? value * 1000 : value).toLocaleString(undefined, {
    month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit',
  });
}

function StatCard({ label, value, detail, icon: Icon }: { label: string; value: string; detail?: string; icon: React.ElementType }) {
  return <section className="glass-bg rounded-xl p-4 sm:p-5">
    <div className="flex items-start justify-between gap-3"><p className="text-xs font-medium uppercase tracking-wider text-text-secondary">{label}</p><Icon size={18} className="text-primary" aria-hidden="true" /></div>
    <p className="mt-3 text-2xl font-semibold tracking-tight sm:text-3xl">{value}</p>
    {detail && <p className="mt-1 text-xs text-text-muted">{detail}</p>}
  </section>;
}

export const Dashboard: React.FC = () => {
  const { principal, logout, isAdmin } = useAuth();
  const [status, setStatus] = useState<Awaited<ReturnType<typeof getConfigStatus>> | null>(null);
  const [summary, setSummary] = useState<Record<string, number> | null>(null);
  const [activity, setActivity] = useState<UsageRecord[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const load = useCallback(async () => {
    setLoading(true); setError(null);
    const [config, usage, recent] = await Promise.allSettled([
      isAdmin ? getConfigStatus() : Promise.resolve(null), getUsageSummary(), getRecentUsage(),
    ]);
    if (config.status === 'fulfilled' && config.value) setStatus(config.value);
    if (usage.status === 'fulfilled') setSummary(usage.value);
    if (recent.status === 'fulfilled') setActivity(recent.value);
    const failures = [config, usage, recent].filter((result) => result.status === 'rejected');
    if (failures.length === 3 || (usage.status === 'rejected' && recent.status === 'rejected')) setError('Unable to load dashboard data. Check the gateway connection and try again.');
    setLoading(false);
  }, [isAdmin]);
  useEffect(() => { void load(); }, [load]);

  const requests = summary?.requests ?? 0;
  const errors = summary?.errors ?? 0;
  const successRate = requests ? `${Math.max(0, ((requests - errors) / requests) * 100).toFixed(1)}%` : '—';
  const cards = isAdmin ? [
    ['Providers', status?.providerCount, 'configured upstreams', Server], ['MCP servers', status?.mcpServerCount, 'tool integrations', Server], ['Model aliases', status?.modelAliasCount, 'routing aliases', Boxes],
    ['API keys', status?.keyCount, 'managed credentials', KeyRound], ['Quotas', status?.quotaCount, 'active limits', ShieldCheck], ['Requests', requests, 'recorded requests', Activity],
  ] as const : [['Requests', requests, 'your recorded requests', Activity]] as const;
  return (
    <div className="min-w-0 space-y-6">
      <header className="border-b border-border bg-slate-950/70 px-4 py-4 sm:px-8"><div className="mx-auto flex max-w-6xl items-center justify-between gap-4">
        <div><p className="text-xs font-medium uppercase tracking-[0.2em] text-primary">Trefoil control plane</p><h1 className="mt-1 font-heading text-2xl font-semibold tracking-tight sm:text-3xl">Overview</h1><p className="mt-1 text-sm text-text-secondary">Gateway health and request activity at a glance</p></div>
        <div className="flex items-center gap-2"><button onClick={() => void load()} disabled={loading} className="inline-flex items-center gap-2 rounded-md border border-border-2 px-3 py-2 text-sm text-text-secondary transition hover:bg-slate-800 hover:text-text disabled:opacity-50" aria-label="Refresh dashboard"><RefreshCw size={15} className={loading ? 'animate-spin' : ''} /> <span className="hidden sm:inline">Refresh</span></button><button onClick={logout} className="inline-flex items-center gap-2 rounded-md px-2 py-2 text-sm text-text-secondary transition hover:bg-slate-800 hover:text-text" aria-label="Log out"><LogOut size={16} /></button></div>
      </div></header>
      <main className="mx-auto max-w-6xl space-y-6 px-4 py-6 sm:px-8 sm:py-8">
        <div className="flex items-center gap-2 text-sm text-text-secondary"><ShieldCheck size={16} className="text-emerald-400" /> Signed in as <span className="font-medium text-text">{principal?.role === 'admin' ? 'Administrator' : principal?.keyName}</span>{!isAdmin && <span className="rounded-full bg-primary/10 px-2 py-0.5 text-xs text-primary">Limited access</span>}</div>
        <div className="flex flex-wrap gap-2"><Link to="/analytics?tab=usage" className="inline-flex items-center gap-2 rounded-md border border-border px-3 py-2 text-sm hover:bg-secondary">Usage Analytics</Link><Link to="/analytics?tab=performance" className="inline-flex items-center gap-2 rounded-md border border-border px-3 py-2 text-sm hover:bg-secondary">Performance</Link>{isAdmin && <Link to="/metrics" className="inline-flex items-center gap-2 rounded-md border border-border px-3 py-2 text-sm hover:bg-secondary">Prometheus metrics</Link>}</div>
        {error && <div role="alert" className="flex items-center gap-3 rounded-lg border border-red-500/30 bg-red-500/10 p-3 text-sm text-red-200"><AlertTriangle size={17} />{error}</div>}
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-5">{cards.map(([label, value, detail, Icon]) => ['Providers', 'MCP servers', 'Model aliases', 'API keys', 'Quotas', 'Requests'].includes(label) ? <Link key={label} to={label === 'Providers' ? '/providers' : label === 'MCP servers' ? '/mcp' : label === 'Model aliases' ? '/aliases' : label === 'API keys' ? '/keys' : label === 'Quotas' ? '/quotas' : '/usage'} className="transition hover:-translate-y-0.5"><StatCard label={label} value={loading ? '…' : value == null ? '—' : number.format(value)} detail={detail} icon={Icon} /></Link> : <StatCard key={label} label={label} value={loading ? '…' : value == null ? '—' : number.format(value)} detail={detail} icon={Icon} />)}</div>
        <div className="grid gap-4 lg:grid-cols-3"><section className="glass-bg rounded-xl p-4 sm:p-5"><div className="flex items-center gap-2"><BarChart3 size={18} className="text-primary" /><h2 className="font-heading text-lg font-semibold">Usage summary</h2></div><div className="mt-5 grid grid-cols-2 gap-4"><div><p className="text-xs text-text-secondary">Success rate</p><p className="mt-1 text-xl font-semibold">{loading ? '…' : successRate}</p></div><div><p className="text-xs text-text-secondary">Errors</p><p className="mt-1 text-xl font-semibold">{loading ? '…' : number.format(errors)}</p></div><div><p className="text-xs text-text-secondary">Input tokens</p><p className="mt-1 text-xl font-semibold">{loading ? '…' : number.format(summary?.inputTokens ?? 0)}</p></div><div><p className="text-xs text-text-secondary">Output tokens</p><p className="mt-1 text-xl font-semibold">{loading ? '…' : number.format(summary?.outputTokens ?? 0)}</p></div></div><p className="mt-5 flex items-center gap-2 border-t border-border pt-4 text-sm text-text-secondary"><CircleDollarSign size={16} /> Total cost: <span className="font-medium text-text">{loading ? '…' : money.format(summary?.totalCost ?? 0)}</span></p></section>
        <section className="glass-bg rounded-xl p-4 sm:col-span-2 sm:p-5"><div className="flex items-center justify-between"><div className="flex items-center gap-2"><Clock3 size={18} className="text-primary" /><h2 className="font-heading text-lg font-semibold">Recent activity</h2></div><span className="text-xs text-text-muted">Latest requests</span></div>{loading ? <p className="py-10 text-center text-sm text-text-muted">Loading activity…</p> : activity.length === 0 ? <div className="py-10 text-center"><TriangleAlert size={22} className="mx-auto text-text-muted" /><p className="mt-2 text-sm text-text-secondary">No requests recorded yet</p></div> : <div className="mt-4 divide-y divide-border">{activity.map((item) => <div key={item.requestId} className="flex items-center justify-between gap-3 py-3 text-sm"><div className="min-w-0"><p className="truncate font-medium">{item.incomingModelAlias || item.provider || 'Unknown route'}</p><p className="mt-0.5 text-xs text-text-muted">{displayTime(item.startTime, item.date)}{item.durationMs != null ? ` · ${item.durationMs}ms` : ''}</p></div><span className={item.responseStatus === 'success' ? 'text-emerald-400' : 'text-red-300'}>{item.responseStatus === 'success' ? 'Success' : item.responseStatus || 'Pending'}</span></div>)}</div>}</section></div>
      </main>
    </div>
  );
};
