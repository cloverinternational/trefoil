import React from 'react';
import { useCallback, useEffect, useState } from 'react';
import { Activity, AlertTriangle, BarChart3, Boxes, CircleDollarSign, Clock3, KeyRound, RefreshCw, Server, ShieldCheck, TriangleAlert } from 'lucide-react';
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
  return <section className="border bg-card p-2">
    <div className="flex items-start justify-between gap-3"><p className="text-xs font-medium uppercase tracking-wider text-muted-foreground text-[10px]">{label}</p><Icon size={14} className="text-primary" aria-hidden="true" /></div>
    <p className="stat-num mt-1 text-lg font-semibold leading-none">{value}</p>
    {detail && <p className="mt-1 text-[11px] text-muted-foreground">{detail}</p>}
  </section>;
}

export const Dashboard: React.FC = () => {
  const { principal, isAdmin } = useAuth();
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
    <div className="min-w-0 space-y-3">
      <header className="flex flex-wrap items-center justify-between gap-2 border-b border-border pb-2">
        <div className="flex items-center gap-3"><h1 className="display">Overview</h1><div className="flex items-center gap-2 text-xs text-muted-foreground"><ShieldCheck size={14} className="text-success" />Signed in as <span className="font-medium text-foreground">{principal?.role === 'admin' ? 'Administrator' : principal?.keyName}</span>{!isAdmin && <span className="bg-primary/10 px-1.5 text-[11px] text-primary">Limited access</span>}</div></div>
        <div className="flex items-center gap-1">
          <Link to="/analytics?tab=usage" className="inline-flex h-7 items-center border border-border px-2 text-xs hover:bg-secondary">Usage analytics</Link>
          <Link to="/analytics?tab=performance" className="inline-flex h-7 items-center border border-border px-2 text-xs hover:bg-secondary">Performance</Link>
          {isAdmin && <Link to="/metrics" className="inline-flex h-7 items-center border border-border px-2 text-xs hover:bg-secondary">Prometheus</Link>}
          <button onClick={() => void load()} disabled={loading} className="inline-flex h-7 cursor-pointer items-center gap-1.5 border border-border px-2 text-xs text-muted-foreground hover:bg-secondary hover:text-foreground disabled:opacity-50" aria-label="Refresh dashboard"><RefreshCw size={12} className={loading ? 'animate-spin' : ''} />Refresh</button>
        </div>
      </header>
      <main className="space-y-3">
        {error && <div role="alert" className="flex items-center gap-3  border border-destructive/40 bg-destructive/10 p-2 text-xs text-destructive"><AlertTriangle size={17} />{error}</div>}
        <div className="grid grid-cols-2 gap-2 md:grid-cols-3 xl:grid-cols-6">{cards.map(([label, value, detail, Icon]) => ['Providers', 'MCP servers', 'Model aliases', 'API keys', 'Quotas', 'Requests'].includes(label) ? <Link key={label} to={label === 'Providers' ? '/providers' : label === 'MCP servers' ? '/mcp' : label === 'Model aliases' ? '/aliases' : label === 'API keys' ? '/keys' : label === 'Quotas' ? '/quotas' : '/usage'} className="block hover:bg-secondary"><StatCard label={label} value={loading ? '…' : value == null ? '—' : number.format(value)} detail={detail} icon={Icon} /></Link> : <StatCard key={label} label={label} value={loading ? '…' : value == null ? '—' : number.format(value)} detail={detail} icon={Icon} />)}</div>
        <div className="grid gap-2 lg:grid-cols-3"><section className="border bg-card p-2"><div className="flex items-center gap-2"><BarChart3 size={14} className="text-primary" /><h2 className="text-xs font-semibold">Usage summary</h2></div><div className="mt-3 grid grid-cols-2 gap-2"><div><p className="text-xs text-muted-foreground">Success rate</p><p className="mt-1 text-base font-semibold">{loading ? '…' : successRate}</p></div><div><p className="text-xs text-muted-foreground">Errors</p><p className="mt-1 text-base font-semibold">{loading ? '…' : number.format(errors)}</p></div><div><p className="text-xs text-muted-foreground">Input tokens</p><p className="mt-1 text-base font-semibold">{loading ? '…' : number.format(summary?.inputTokens ?? 0)}</p></div><div><p className="text-xs text-muted-foreground">Output tokens</p><p className="mt-1 text-base font-semibold">{loading ? '…' : number.format(summary?.outputTokens ?? 0)}</p></div></div><p className="mt-3 flex items-center gap-2 border-t border-border pt-2 text-xs text-muted-foreground"><CircleDollarSign size={16} /> Total cost: <span className="font-medium text-foreground">{loading ? '…' : money.format(summary?.totalCost ?? 0)}</span></p></section>
        <section className="border bg-card  p-4 lg:col-span-2 "><div className="flex items-center justify-between"><div className="flex items-center gap-2"><Clock3 size={14} className="text-primary" /><h2 className="text-xs font-semibold">Recent activity</h2></div><span className="text-xs text-muted-foreground">Latest requests</span></div>{loading ? <p className="py-6 text-center text-xs text-muted-foreground">Loading activity…</p> : activity.length === 0 ? <div className="py-6 text-center"><TriangleAlert size={22} className="mx-auto text-muted-foreground" /><p className="mt-2 text-xs text-muted-foreground">No requests recorded yet</p></div> : <div className="mt-2 divide-y divide-border">{activity.map((item) => <div key={item.requestId} className="flex items-center justify-between gap-3 py-1.5 text-xs"><div className="min-w-0"><p className="truncate font-medium">{item.incomingModelAlias || item.provider || 'Unknown route'}</p><p className="mt-0.5 text-xs text-muted-foreground">{displayTime(item.startTime, item.date)}{item.durationMs != null ? ` · ${item.durationMs}ms` : ''}</p></div><span className={item.responseStatus === 'success' ? 'text-success' : 'text-destructive'}>{item.responseStatus === 'success' ? 'Success' : item.responseStatus || 'Pending'}</span></div>)}</div>}</section></div>
      </main>
    </div>
  );
};
