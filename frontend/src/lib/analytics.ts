const headers = () => ({ 'x-admin-key': localStorage.getItem('trefoil_admin_key') ?? '', 'content-type': 'application/json' });

export type AnalyticsRange = 'hour' | 'day' | 'week' | 'month';
export type AnalyticsMetric = 'requests' | 'inputTokens' | 'outputTokens' | 'tokens' | 'cost' | 'errors' | 'avgTps' | 'avgTtftMs' | 'avgDurationMs';
export type AnalyticsDimension = 'model' | 'provider' | 'apiKey' | 'permissionGroup' | 'time';
export interface AnalyticsQuery { metric: string; dimension: string; filters: { dimension: string; value: string }[]; start: number; end: number; bucket?: string; formula?: string; }
export interface AnalyticsPoint { label: string | null; bucketStartMs?: number; value: number | null; samples?: number; }
export interface AnalyticsResult { data: AnalyticsPoint[]; unit?: string; }
export interface AnalyticsPanel { id: string; title: string; metric: string; dimension: string; formula?: string; }
export interface AnalyticsDashboardDefinition { id: string; name: string; panels: AnalyticsPanel[]; range?: AnalyticsRange; filters?: Record<string, string>; version?: number; }

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(path, { ...init, headers: { ...headers(), ...(init?.headers ?? {}) } });
  if (!response.ok) {
    const body = await response.json().catch(() => null) as { error?: string | { message?: string } } | null;
    throw new Error(typeof body?.error === 'string' ? body.error : body?.error?.message || `Analytics request failed (${response.status})`);
  }
  return response.json() as Promise<T>;
}

type StoredDashboard = { id: string; name: string; config: { panels: AnalyticsPanel[]; range?: AnalyticsRange; filters?: Record<string, string> }; version: number };
const unpack = (stored: StoredDashboard): AnalyticsDashboardDefinition => ({ id: stored.id, name: stored.name, panels: stored.config.panels, range: stored.config.range, filters: stored.config.filters, version: stored.version });
export async function queryAnalytics(query: AnalyticsQuery): Promise<AnalyticsResult> {
  const result = await request<{ data: { label: string | number | null; value: number | null; samples: number; bucketStartMs?: number }[]; unit?: string }>('/v0/management/analytics/query', { method: 'POST', body: JSON.stringify(query) });
  return { ...result, data: result.data.map(point => ({ label: point.label == null ? null : String(point.label), bucketStartMs: point.bucketStartMs ?? (Number(point.label) || undefined), value: point.value, samples: point.samples })) };
}
export async function listAnalyticsDashboards(): Promise<AnalyticsDashboardDefinition[]> { return (await request<StoredDashboard[]>('/v0/management/analytics/dashboards')).map(unpack); }
export async function createAnalyticsDashboard(dashboard: Omit<AnalyticsDashboardDefinition, 'id' | 'version'>): Promise<AnalyticsDashboardDefinition> {
  const result = await request<{ id: string; version: number }>('/v0/management/analytics/dashboards', { method: 'POST', body: JSON.stringify({ name: dashboard.name, config: { panels: dashboard.panels, range: dashboard.range, filters: dashboard.filters } }) });
  return { ...dashboard, ...result };
}
export async function getAnalyticsDashboard(id: string): Promise<AnalyticsDashboardDefinition> { return unpack(await request<StoredDashboard>(`/v0/management/analytics/dashboards/${encodeURIComponent(id)}`)); }
export async function updateAnalyticsDashboard(id: string, dashboard: Omit<AnalyticsDashboardDefinition, 'id' | 'version'> & { version: number }): Promise<AnalyticsDashboardDefinition> {
  const result = await request<{ id: string; version: number }>(`/v0/management/analytics/dashboards/${encodeURIComponent(id)}`, { method: 'PUT', body: JSON.stringify({ name: dashboard.name, config: { panels: dashboard.panels, range: dashboard.range, filters: dashboard.filters }, version: dashboard.version }) });
  return { ...dashboard, ...result };
}
export const deleteAnalyticsDashboard = (id: string) => request<void>(`/v0/management/analytics/dashboards/${encodeURIComponent(id)}`, { method: 'DELETE' });
