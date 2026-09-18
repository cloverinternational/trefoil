import type { CostDisplay, AggregatedPoint, ChartType, DetailedUsagePreset, GroupBy, MetricConfig, MetricKey, TimeRange, ViewMode } from './types';

export interface CustomDateRange { start: Date; end: Date }
export const COLORS = ['#3b82f6', '#10b981', '#f59e0b', '#8b5cf6', '#ec4899', '#06b6d4', '#f97316', '#84cc16'];
export const LIVE_WINDOW_MINUTES = 5;
export const DEFAULT_METRICS: MetricKey[] = ['requests', 'tokens', 'cost'];
export const VALID_METRIC_KEYS: readonly MetricKey[] = ['requests', 'tokens', 'cost', 'duration', 'ttft', 'tps', 'errors', 'velocity'];
const ranges: TimeRange[] = ['live', 'hour', 'day', 'week', 'month', 'custom'];
const charts: ChartType[] = ['line', 'bar', 'area', 'pie', 'composed'];
const groups: GroupBy[] = ['time', 'provider', 'model', 'apiKey', 'status'];
const views: ViewMode[] = ['chart', 'list'];

const formatNumber = (value: number, decimals = 0) => value.toLocaleString(undefined, { minimumFractionDigits: decimals, maximumFractionDigits: decimals });
const formatTokens = (value: number) => formatNumber(value);
const formatMs = (value: number) => `${formatNumber(value, 0)}ms`;
const formatCost = (value: number, display: CostDisplay) => new Intl.NumberFormat(undefined, { style: 'currency', currency: display.currency, minimumFractionDigits: 4, maximumFractionDigits: 4 }).format(value * display.rate);

export const createMetrics = ({ currency, rate, symbol }: CostDisplay): MetricConfig[] => [
  { key: 'requests', label: 'Requests', color: COLORS[0], yAxisId: 'left', format: value => formatNumber(value) },
  { key: 'tokens', label: 'Tokens', color: COLORS[1], yAxisId: 'right', format: formatTokens },
  { key: 'cost', label: `Cost (${symbol})`, color: COLORS[2], yAxisId: 'right', format: value => formatCost(value, { currency, rate, symbol }) },
  { key: 'duration', label: 'Duration', color: COLORS[3], yAxisId: 'right', format: formatMs },
  { key: 'ttft', label: 'TTFT', color: COLORS[4], yAxisId: 'right', format: formatMs },
  { key: 'tps', label: 'TPS', color: COLORS[5], yAxisId: 'right', format: value => formatNumber(value, 1) },
  { key: 'errors', label: 'Errors', color: '#ef4444', yAxisId: 'left', format: formatNumber },
];

export const parsePresetFromQuery = (query: string): DetailedUsagePreset => {
  const params = new URLSearchParams(query);
  const requested = (params.get('metrics') ?? params.get('metric') ?? DEFAULT_METRICS.join(',')).split(',');
  const selected = requested.map(value => value.trim()).filter((value): value is MetricKey => VALID_METRIC_KEYS.includes(value as MetricKey));
  const range = params.get('range');
  const start = params.get('startDate'); const end = params.get('endDate');
  return { timeRange: ranges.includes(range as TimeRange) ? range as TimeRange : 'day', chartType: charts.includes(params.get('chartType') as ChartType) ? params.get('chartType') as ChartType : 'area', groupBy: groups.includes(params.get('groupBy') as GroupBy) ? params.get('groupBy') as GroupBy : 'time', viewMode: views.includes(params.get('viewMode') as ViewMode) ? params.get('viewMode') as ViewMode : 'chart', selectedMetrics: selected.length ? selected : [...DEFAULT_METRICS], customStartDate: range === 'custom' && start && end ? start : undefined, customEndDate: range === 'custom' && start && end ? end : undefined };
};

export const parseCustomDateRange = (startDate?: string, endDate?: string): CustomDateRange | null => {
  if (!startDate || !endDate) return null;
  const start = new Date(startDate); const end = new Date(endDate);
  return Number.isNaN(start.getTime()) || Number.isNaN(end.getTime()) ? null : { start, end };
};

export const getRangeConfig = (range: TimeRange, customRange?: CustomDateRange | null) => {
  if (range === 'custom' && customRange) {
    const minutes = (customRange.end.getTime() - customRange.start.getTime()) / 60000;
    let bucket = minutes <= 30 ? 1 : minutes <= 1440 ? 5 : minutes <= 10080 ? 60 : 360;
    bucket = Math.max(bucket, Math.ceil(minutes / 100));
    return { minutes, bucketFn: (timestampMs: number) => Math.floor(timestampMs / (bucket * 60000)) * bucket * 60000 };
  }
  if (range === 'live' || range === 'hour') return { minutes: range === 'live' ? LIVE_WINDOW_MINUTES : 60, bucketFn: (t: number) => Math.floor(t / 60000) * 60000 };
  if (range === 'day') return { minutes: 1440, bucketFn: (t: number) => Math.floor(t / 3600000) * 3600000 };
  return { minutes: range === 'week' ? 10080 : 43200, bucketFn: (t: number) => { const date = new Date(t); date.setHours(0, 0, 0, 0); return date.getTime(); } };
};

export const formatBucketLabel = (range: TimeRange, milliseconds: number, customRange?: CustomDateRange | null) => {
  const date = new Date(milliseconds); const customMinutes = customRange ? (customRange.end.getTime() - customRange.start.getTime()) / 60000 : Infinity;
  return (range === 'custom' ? customMinutes <= 1440 : ['live', 'hour', 'day'].includes(range)) ? date.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }) : date.toLocaleDateString();
};
export const calcVelocity = (data: AggregatedPoint[]) => data.map((point, index) => ({ ...point, velocity: index ? point.requests - data[index - 1].requests : 0 }));
export const getSummaryBreakdown = (groupBy: GroupBy, viewMode: ViewMode) => groupBy === 'time' || viewMode === 'list' ? [] : [groupBy === 'model' ? 'modelAlias' : groupBy];
