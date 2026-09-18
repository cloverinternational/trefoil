import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { Activity, AlertTriangle, ArrowLeft, BarChart3, RefreshCw, Server } from 'lucide-react';
import { Link } from 'react-router-dom';
import { Navigation } from '../components/layout/Navigation';
import { Button } from '../components/ui/Button';
import { useAuth } from '../contexts/AuthContext';
import { getMetrics } from '../lib/api';

type Metric = { name: string; labels: Record<string, string>; value: number };
function parseMetrics(text: string): Metric[] {
  const result: Metric[] = [];
  for (const line of text.split('\n')) {
    const match = line.match(/^([a-zA-Z_:][a-zA-Z0-9_:]*)(?:\{([^}]*)\})?\s+([-+0-9.eE]+)$/);
    if (!match) continue;
    const labels: Record<string, string> = {};
    for (const item of (match[2] ?? '').matchAll(/([a-zA-Z_][a-zA-Z0-9_]*)="((?:\\.|[^"\\])*)"/g)) labels[item[1]] = item[2];
    const value = Number(match[3]);
    if (Number.isFinite(value)) result.push({ name: match[1], labels, value });
  }
  return result;
}
const number = new Intl.NumberFormat();
const valueOf = (metrics: Metric[], name: string) => metrics.find(metric => metric.name === name)?.value ?? 0;
export const Metrics: React.FC = () => {
  const { isAdmin } = useAuth(); const [raw, setRaw] = useState(''); const [loading, setLoading] = useState(true); const [error, setError] = useState<string | null>(null);
  const load = useCallback(async () => { setLoading(true); setError(null); try { setRaw(await getMetrics()); } catch (cause) { setError(cause instanceof Error ? cause.message : 'Unable to load metrics'); } finally { setLoading(false); } }, []);
  useEffect(() => { if (isAdmin) void load(); else setLoading(false); }, [isAdmin, load]);
  const metrics = useMemo(() => parseMetrics(raw), [raw]); const providers = metrics.filter(metric => metric.name === 'plexus_provider_requests_total');
  if (!isAdmin) return <Shell><section className="glass-bg rounded-xl p-6 text-center"><AlertTriangle className="mx-auto text-amber-300" /><h2 className="mt-4 font-heading text-2xl font-semibold">Administrator access required</h2><p className="mt-2 text-text-secondary">Operational metrics are available only to administrators.</p></section></Shell>;
  return <Shell><div className="flex justify-end"><Button variant="secondary" size="sm" onClick={() => void load()} disabled={loading} leftIcon={<RefreshCw size={14} className={loading ? 'animate-spin' : ''} />}>Refresh</Button></div>{error && <div role="alert" className="rounded-lg border border-red-500/30 bg-red-500/10 p-3 text-sm text-red-200">{error}</div>}{loading ? <div className="glass-bg rounded-xl py-16 text-center text-sm text-text-muted" role="status">Loading metrics…</div> : <><div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4"><Card icon={Activity} label="Requests" value={valueOf(metrics, 'plexus_requests_total')} /><Card icon={AlertTriangle} label="Errors" value={valueOf(metrics, 'plexus_errors_total')} /><Card icon={BarChart3} label="Requests today" value={valueOf(metrics, 'plexus_requests_today')} /><Card icon={Server} label="Avg. duration" value={valueOf(metrics, 'plexus_avg_duration_ms')} suffix=" ms" /></div><section className="glass-bg overflow-hidden rounded-xl"><div className="border-b border-border p-5"><h2 className="font-heading text-lg font-semibold">Provider activity</h2><p className="mt-1 text-sm text-text-secondary">Cumulative request counts from durable usage records.</p></div>{providers.length === 0 ? <p className="p-8 text-center text-sm text-text-muted">No provider activity recorded yet.</p> : <div className="divide-y divide-border">{providers.map(provider => <div key={provider.labels.provider} className="flex items-center justify-between px-5 py-4"><span className="font-mono text-sm">{provider.labels.provider || 'unknown'}</span><span className="text-sm text-text-secondary">{number.format(provider.value)} requests</span></div>)}</div>}</section></>}</Shell>;
};
const Card: React.FC<{ icon: React.ElementType; label: string; value: number; suffix?: string }> = ({ icon: Icon, label, value, suffix = '' }) => <section className="glass-bg rounded-xl p-4"><div className="flex items-center justify-between"><p className="text-xs font-medium uppercase tracking-wider text-text-secondary">{label}</p><Icon size={17} className="text-primary" /></div><p className="mt-3 text-2xl font-semibold">{number.format(value)}{suffix}</p></section>;
const Shell: React.FC<{ children: React.ReactNode }> = ({ children }) => <div className="min-h-screen bg-bg-deep text-text"><header className="border-b border-border bg-slate-950/70 px-4 py-5 sm:px-8"><div className="mx-auto max-w-6xl"><Link to="/" className="text-sm text-text-secondary">← Back to overview</Link><h1 className="mt-3 font-heading text-2xl font-semibold">Metrics</h1><p className="mt-1 text-sm text-text-secondary">Gateway performance and provider activity</p></div></header><main className="mx-auto max-w-6xl space-y-5 px-4 py-6 sm:px-8"><Navigation />{children}<Link to="/" className="inline-flex items-center gap-2 text-sm text-text-secondary hover:text-text"><ArrowLeft size={15} /> Back to overview</Link></main></div>;
