import React, { useCallback, useEffect, useState } from 'react';
import { Link, useSearchParams } from 'react-router-dom';
import { Activity, BarChart3, Gauge, Plus, RefreshCw, Save, Trash2 } from 'lucide-react';
import { useAuth } from '../contexts/AuthContext';
import { Button } from '../components/ui/button';
import { Card, CardContent } from '../components/ui/card';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '../components/ui/select';
import { AnalyticsPanelView, AddPanelButton, newAnalyticsPanel } from '../components/analytics';
import { ProviderPerformanceSamples } from '../components/analytics/ProviderPerformanceSamples';
import { createAnalyticsDashboard, deleteAnalyticsDashboard, listAnalyticsDashboards, updateAnalyticsDashboard, type AnalyticsDashboardDefinition, type AnalyticsPanel, type AnalyticsRange } from '../lib/analytics';

const ranges: { value: AnalyticsRange; label: string }[] = [{ value: 'hour', label: 'Hour' }, { value: 'day', label: 'Day' }, { value: 'week', label: 'Week' }, { value: 'month', label: 'Month' }];
const validFilters = (filters?: Record<string, string>) => Object.fromEntries(Object.entries(filters ?? {}).filter(([dimension, value]) => ['model', 'provider', 'apiKey', 'permissionGroup'].includes(dimension) && typeof value === 'string'));
const starterPanels = (performance: boolean): AnalyticsPanel[] => performance
  ? [newAnalyticsPanel('avgTps', 'provider'), newAnalyticsPanel('avgTtftMs', 'provider'), newAnalyticsPanel('avgTps', 'model'), newAnalyticsPanel('avgTtftMs', 'model'), newAnalyticsPanel('avgTps', 'permissionGroup'), newAnalyticsPanel('avgTtftMs', 'permissionGroup')]
  : [newAnalyticsPanel('requests', 'time'), newAnalyticsPanel('tokens', 'time'), newAnalyticsPanel('requests', 'model'), newAnalyticsPanel('outputTokens', 'model'), newAnalyticsPanel('requests', 'provider'), newAnalyticsPanel('tokens', 'apiKey'), newAnalyticsPanel('avgTps', 'permissionGroup'), newAnalyticsPanel('avgTtftMs', 'permissionGroup')];

export const AnalyticsDashboard: React.FC = () => {
  const { isAdmin } = useAuth();
  const [params, setParams] = useSearchParams();
  const tab = params.get('tab') === 'performance' ? 'performance' : 'usage';
  const [range, setRange] = useState<AnalyticsRange>('day');
  const [filters, setFilters] = useState<Record<string, string>>({});
  const [panels, setPanels] = useState<AnalyticsPanel[]>(() => starterPanels(tab === 'performance'));
  const [dashboards, setDashboards] = useState<AnalyticsDashboardDefinition[]>([]);
  const [selected, setSelected] = useState('default');
  const [name, setName] = useState(tab === 'performance' ? 'Performance' : 'Usage Analytics');
  const [loadingDashboards, setLoadingDashboards] = useState(true);
  const [saving, setSaving] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const editable = isAdmin;

  useEffect(() => {
    setSelected('default');
    setPanels(starterPanels(tab === 'performance'));
    setName(tab === 'performance' ? 'Performance' : 'Usage Analytics');
    setFilters({});
  }, [tab]);

  const reload = useCallback(async () => {
    setLoadingDashboards(true); setError(null);
    try { setDashboards(await listAnalyticsDashboards()); }
    catch (reason) { setError(reason instanceof Error ? reason.message : 'Unable to load saved dashboards'); }
    finally { setLoadingDashboards(false); }
  }, []);
  useEffect(() => { void reload(); }, [reload]);

  const selectDashboard = (id: string) => {
    setSelected(id); setNotice(null);
    if (id === 'default') {
      setName(tab === 'performance' ? 'Performance' : 'Usage Analytics');
      setPanels(starterPanels(tab === 'performance'));
      setRange('day'); setFilters({});
    } else {
      const dashboard = dashboards.find(item => item.id === id);
      if (dashboard) { setName(dashboard.name); setPanels(dashboard.panels); setRange(dashboard.range === 'hour' || dashboard.range === 'week' || dashboard.range === 'month' ? dashboard.range : 'day'); setFilters(validFilters(dashboard.filters)); }
    }
  };
  const save = async () => {
    setSaving(true); setError(null); setNotice(null);
    try {
      const definition = { name: name.trim() || 'Untitled dashboard', panels, range, filters };
      const currentVersion = dashboards.find(item => item.id === selected)?.version;
      const saved = selected === 'default' ? await createAnalyticsDashboard(definition) : await updateAnalyticsDashboard(selected, { ...definition, version: currentVersion ?? 0 });
      setSelected(saved.id); setName(saved.name); setPanels(saved.panels); setRange(saved.range ?? range); setFilters(validFilters(saved.filters ?? filters)); setNotice('Dashboard saved.'); await reload();
    } catch (reason) { setError(reason instanceof Error ? reason.message : 'Unable to save dashboard'); }
    finally { setSaving(false); }
  };
  const remove = async () => {
    if (selected === 'default' || !window.confirm(`Delete “${name}”?`)) return;
    try { await deleteAnalyticsDashboard(selected); setSelected('default'); setName(tab === 'performance' ? 'Performance' : 'Usage Analytics'); setPanels(starterPanels(tab === 'performance')); setRange('day'); setFilters({}); setNotice('Dashboard deleted.'); await reload(); }
    catch (reason) { setError(reason instanceof Error ? reason.message : 'Unable to delete dashboard'); }
  };
  const move = (index: number, direction: -1 | 1) => setPanels(current => {
    const target = index + direction;
    if (target < 0 || target >= current.length) return current;
    const next = [...current]; [next[index], next[target]] = [next[target], next[index]]; return next;
  });
  const title = tab === 'performance' ? 'Performance' : 'Usage Analytics';

  return <main className="min-w-0 space-y-5">
    <div className="-mx-4 border-b bg-card px-4 pt-4 md:-mx-6 md:px-6">
      <div><h1 className="font-heading text-2xl font-semibold">{title}</h1><p className="mt-1 text-sm text-muted-foreground">Explore usage and model performance across your gateway.</p></div>
      <nav aria-label="Analytics tabs" className="mt-4 flex gap-5 overflow-x-auto text-sm">
        <button onClick={() => setParams({ tab: 'usage' })} aria-current={tab === 'usage' ? 'page' : undefined} className={`flex shrink-0 items-center gap-2 border-b-2 px-1 pb-3 ${tab === 'usage' ? 'border-primary font-semibold text-foreground' : 'border-transparent text-muted-foreground'}`}><BarChart3 size={15}/>Usage Analytics</button>
        <button onClick={() => setParams({ tab: 'performance' })} aria-current={tab === 'performance' ? 'page' : undefined} className={`flex shrink-0 items-center gap-2 border-b-2 px-1 pb-3 ${tab === 'performance' ? 'border-primary font-semibold text-foreground' : 'border-transparent text-muted-foreground'}`}><Gauge size={15}/>Performance</button>
      </nav>
    </div>
    <div className="flex min-w-0 flex-wrap items-end justify-between gap-3">
      <div className="flex min-w-0 flex-wrap items-center gap-2">
        <label className="eyebrow" htmlFor="analytics-range">Range</label><Select value={range} onValueChange={value => setRange(value as AnalyticsRange)}><SelectTrigger id="analytics-range" className="w-28"><SelectValue/></SelectTrigger><SelectContent>{ranges.map(item => <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>)}</SelectContent></Select>
        {([['model', 'Model', 'All models'], ['provider', 'Provider', 'All providers'], ['permissionGroup', 'Permission group', 'All groups'], ['apiKey', 'API key', 'All keys']] as const).map(([dimension, label, placeholder]) => <React.Fragment key={dimension}><label className="eyebrow ml-2" htmlFor={`analytics-${dimension}`}>{label}</label><input id={`analytics-${dimension}`} aria-label={`${label} filter`} placeholder={placeholder} value={filters[dimension] ?? ''} onChange={event => setFilters(current => ({ ...current, [dimension]: event.target.value }))} className="h-9 w-36 rounded-md border border-input bg-transparent px-3 text-sm" /></React.Fragment>)}
      </div>
      <Button variant="outline" onClick={() => window.location.reload()}><RefreshCw/>Refresh</Button>
    </div>
    {error && <div role="alert" className="rounded-md border border-destructive/40 bg-destructive/10 p-3 text-sm text-destructive">{error}</div>}
    {notice && <div role="status" className="text-sm text-emerald-500">{notice}</div>}
    <Card><CardContent className="flex min-w-0 flex-wrap items-center gap-2 p-3">
      <label className="sr-only" htmlFor="saved-dashboard">Saved dashboard</label><Select value={selected} onValueChange={selectDashboard} disabled={loadingDashboards}><SelectTrigger id="saved-dashboard" className="w-full sm:w-56"><SelectValue placeholder="Choose dashboard"/></SelectTrigger><SelectContent><SelectItem value="default">{title} (starter)</SelectItem>{dashboards.map(item => <SelectItem key={item.id} value={item.id}>{item.name}</SelectItem>)}</SelectContent></Select>
      {editable && <><input aria-label="Dashboard name" value={name} onChange={event => setName(event.target.value)} className="h-9 min-w-0 flex-1 rounded-md border border-input bg-transparent px-3 text-sm" />
      <AddPanelButton onClick={() => setPanels(current => [...current, newAnalyticsPanel()])}/><Button onClick={() => void save()} disabled={saving || panels.length === 0}><Save/>{saving ? 'Saving…' : 'Save dashboard'}</Button>
      {selected !== 'default' && <Button variant="destructive" aria-label="Delete dashboard" onClick={() => void remove()}><Trash2/></Button>}</>}
    </CardContent></Card>
    <div className="grid min-w-0 gap-4 lg:grid-cols-2">
      {panels.map((panel, index) => <AnalyticsPanelView key={panel.id} panel={panel} range={range} filters={filters} editable={editable} onChange={updated => setPanels(current => current.map(item => item.id === panel.id ? updated : item))} onRemove={() => setPanels(current => current.filter(item => item.id !== panel.id))} onMove={direction => move(index, direction)} />)}
    </div>
    {tab === 'performance' && isAdmin && <ProviderPerformanceSamples provider={filters.provider ?? ''} model={filters.model ?? ''} />}
    {panels.length === 0 && <div className="flex flex-col items-center gap-3 rounded-lg border border-dashed p-10 text-center"><Activity className="text-muted-foreground"/><p className="text-sm text-muted-foreground">This dashboard has no panels yet.</p>{editable && <Button variant="outline" onClick={() => setPanels([newAnalyticsPanel()])}><Plus/>Add your first panel</Button>}</div>}
    <div className="flex flex-wrap gap-4 border-t pt-4 text-sm"><Link to="/detailed-usage" className="text-primary hover:underline">Detailed request log</Link><Link to="/metrics" className="text-primary hover:underline">Prometheus metrics</Link></div>
  </main>;
};
