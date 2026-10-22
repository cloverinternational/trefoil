import React, { useEffect, useState } from 'react';
import ReactECharts from 'echarts-for-react';
import { Activity, ArrowDown, ArrowUp, Plus, Trash2 } from 'lucide-react';
import { Button } from '../ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '../ui/card';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '../ui/select';
import { queryAnalytics, type AnalyticsPanel as PanelDefinition, type AnalyticsPoint, type AnalyticsRange } from '../../lib/analytics';
import { useTheme } from '../../contexts/ThemeContext';

export const METRICS = [
  { value: 'requests', label: 'Requests' }, { value: 'inputTokens', label: 'Input tokens' },
  { value: 'outputTokens', label: 'Output tokens' }, { value: 'tokens', label: 'Total tokens' },
  { value: 'cost', label: 'Cost' }, { value: 'errors', label: 'Errors' },
  { value: 'avgTps', label: 'Average output TPS' }, { value: 'avgTtftMs', label: 'Average first-token latency' },
  { value: 'avgDurationMs', label: 'Average request duration' },
];
export const DIMENSIONS = [
  { value: 'time', label: 'Time' }, { value: 'provider', label: 'Provider' },
  { value: 'model', label: 'Model' }, { value: 'apiKey', label: 'API key' },
  { value: 'permissionGroup', label: 'Permission group' },
];
const rangeMs: Record<AnalyticsRange, number> = { hour: 3_600_000, day: 86_400_000, week: 604_800_000, month: 2_592_000_000 };
const formulaHelp = 'Use a safe arithmetic expression over metric names, e.g. outputTokens / requests. Operators: +, -, *, /, parentheses.';

export function AnalyticsPanelView({ panel, range, filters, editable, onChange, onRemove, onMove }: {
  panel: PanelDefinition; range: AnalyticsRange; filters: Record<string, string>; editable: boolean;
  onChange: (panel: PanelDefinition) => void; onRemove: () => void; onMove: (direction: -1 | 1) => void;
}) {
  const { theme } = useTheme();
  const [data, setData] = useState<AnalyticsPoint[] | null>(null);
  const [unit, setUnit] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  useEffect(() => {
    let current = true;
    setLoading(true); setError(null); setData(null);
    const end = Date.now();
    const duration = rangeMs[range];
    const queryFilters = Object.entries(filters).filter(([, value]) => value.trim()).map(([dimension, value]) => ({ dimension, value: value.trim() }));
    queryAnalytics({ metric: panel.metric, ...(panel.formula?.trim() ? { formula: panel.formula.trim() } : {}), dimension: panel.dimension, filters: queryFilters, start: end - duration, end, ...(panel.dimension === 'time' ? { bucket: range === 'month' ? 'month' : range } : {}) })
      .then(result => { if (current) { setData(result.data); setUnit(result.unit ?? ''); } })
      .catch(reason => { if (current) setError(reason instanceof Error ? reason.message : 'Unable to load panel'); })
      .finally(() => { if (current) setLoading(false); });
    return () => { current = false; };
  }, [panel.metric, panel.dimension, panel.formula, range, JSON.stringify(filters)]);

  const labels = data?.map(point => point.label ?? 'Unknown') ?? [];
  const color = theme === 'dark' ? '#a3a3a3' : '#737373';
  const isTime = panel.dimension === 'time';
  const metricLabel = METRICS.find(metric => metric.value === panel.metric)?.label ?? panel.metric;
  return <Card className="min-w-0 overflow-hidden">
    <CardHeader className="flex-row flex-wrap items-center justify-between gap-2 pb-2">
      <CardTitle className="flex min-w-0 items-center gap-2 text-xs"><Activity size={13} className="shrink-0 text-primary"/><span className="truncate">{editable ? <input aria-label="Panel title" value={panel.title} onChange={event => onChange({ ...panel, title: event.target.value })} className="w-full min-w-24 bg-transparent outline-none"/> : panel.title}</span></CardTitle>
      <div className="flex flex-wrap items-center gap-1">
        {editable && <>
          <Select value={panel.metric} onValueChange={metric => onChange({ ...panel, metric })}><SelectTrigger className="h-7 w-32" aria-label="Metric"><SelectValue/></SelectTrigger><SelectContent>{METRICS.map(item => <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>)}</SelectContent></Select>
          <Select value={panel.dimension} onValueChange={dimension => onChange({ ...panel, dimension })}><SelectTrigger className="h-7 w-32" aria-label="Dimension"><SelectValue/></SelectTrigger><SelectContent>{DIMENSIONS.map(item => <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>)}</SelectContent></Select>
          <label className="sr-only" htmlFor={`formula-${panel.id}`}>Metric formula</label><input id={`formula-${panel.id}`} aria-label="Metric formula" title={formulaHelp} placeholder="Formula (optional)" value={panel.formula ?? ''} onChange={event => onChange({ ...panel, formula: event.target.value })} className="h-8 w-48  border border-input bg-transparent px-2 text-xs" />
          <Button size="icon" variant="ghost" aria-label="Move panel up" onClick={() => onMove(-1)}><ArrowUp/></Button><Button size="icon" variant="ghost" aria-label="Move panel down" onClick={() => onMove(1)}><ArrowDown/></Button><Button size="icon" variant="ghost" aria-label="Remove panel" onClick={onRemove}><Trash2/></Button>
        </>}
      </div>
    </CardHeader>
    <CardContent className="min-w-0">
      {loading ? <div role="status" className="flex h-44 items-center justify-center text-xs text-muted-foreground">Loading {panel.title}…</div>
        : error ? <div role="alert" className="flex h-44 items-center justify-center px-5 text-center text-xs text-destructive">{error}</div>
          : !data?.length ? <div className="flex h-44 items-center justify-center text-xs text-muted-foreground">No {metricLabel.toLowerCase()} data for this selection.</div>
            : <><ReactECharts style={{ height: 176, width: '100%' }} option={{
              backgroundColor: 'transparent', animation: false,
              tooltip: { trigger: 'axis', valueFormatter: (value: unknown) => value == null ? 'Unknown' : `${Number(value).toLocaleString(undefined, { maximumFractionDigits: 3 })}${unit ? ` ${unit}` : ''}` },
              grid: { left: 52, right: 20, top: 22, bottom: 45, containLabel: true },
              xAxis: { type: isTime ? 'category' : 'value', ...(isTime ? { data: labels, boundaryGap: false } : {}), axisLabel: { color, rotate: isTime ? 0 : 0 }, axisLine: { lineStyle: { color } } },
              yAxis: { type: isTime ? 'value' : 'category', ...(isTime ? {} : { data: labels }), axisLabel: { color, width: 120, overflow: 'truncate' }, splitLine: { lineStyle: { color: theme === 'dark' ? '#262626' : '#e5e5e5' } } },
              series: [{ type: isTime ? 'line' : 'bar', smooth: true, data: data.map(point => point.value), color: '#ff8300', connectNulls: false, symbolSize: 5, itemStyle: { borderRadius: 4 }, areaStyle: isTime ? { opacity: 0.12 } : undefined }],
            }} /><div className="mt-3 max-h-36 overflow-y-auto border-t border-border pt-2 text-xs" aria-label={`${panel.title} values`}>{data.map((point, index) => <div key={`${point.label}-${index}`} className="flex justify-between gap-3 py-1"><span className="truncate text-muted-foreground" title={point.label ?? 'Unknown'}>{point.label ?? 'Unknown'}</span><span className="shrink-0 tabular-nums">{point.value == null ? '—' : point.value.toLocaleString(undefined, { maximumFractionDigits: 2 })}{point.value == null || !unit ? '' : ` ${unit}`} <span className="text-muted-foreground">({point.samples ?? 0} samples)</span></span></div>)}</div></>}
    </CardContent>
  </Card>;
}

export function newAnalyticsPanel(metric = 'requests', dimension = 'time'): PanelDefinition {
  const descriptor = METRICS.find(item => item.value === metric);
  return { id: crypto.randomUUID(), title: `${descriptor?.label ?? metric} by ${dimension}`, metric, dimension };
}

export function AddPanelButton({ onClick }: { onClick: () => void }) {
  return <Button variant="outline" onClick={onClick}><Plus/>Add panel</Button>;
}
