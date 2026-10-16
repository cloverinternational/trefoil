import React, { useEffect, useState } from 'react';
import { Card, CardContent, CardHeader } from '../ui/card';
import { getProviderPerformance, type ProviderPerformanceRow } from '../../lib/api';

const display = (value: number, samples: number, unit: string) => samples > 0
  ? `${value.toLocaleString(undefined, { maximumFractionDigits: 2 })} ${unit}`
  : '—';

/** Admin-only sample provenance. Older API averages may be zero with no observations. */
export function ProviderPerformanceSamples({ provider, model }: { provider: string; model: string }) {
  const [rows, setRows] = useState<ProviderPerformanceRow[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let current = true;
    setLoading(true); setError(null);
    getProviderPerformance(provider.trim(), model.trim())
      .then(result => { if (current) setRows(result); })
      .catch(reason => { if (current) setError(reason instanceof Error ? reason.message : 'Unable to load provider performance'); })
      .finally(() => { if (current) setLoading(false); });
    return () => { current = false; };
  }, [provider, model]);
  return <Card><CardHeader><h2 className="font-heading text-base font-semibold">Provider performance samples</h2></CardHeader><CardContent>
    {loading ? <p role="status" className="text-sm text-muted-foreground">Loading provider samples…</p> : error ? <p role="alert" className="text-sm text-destructive">{error}</p> : rows.length === 0 ? <p className="text-sm text-muted-foreground">No provider performance samples for this selection.</p> :
    <div className="overflow-x-auto"><table className="w-full min-w-[620px] text-left text-xs"><thead><tr className="border-b text-muted-foreground"><th className="p-2">Provider / model</th><th className="p-2">TTFT</th><th className="p-2">Generation TPS</th><th className="p-2">E2E TPS</th><th className="p-2">Requests</th></tr></thead><tbody>{rows.map(row => <tr key={`${row.provider}/${row.model}`} className="border-b last:border-0"><td className="p-2 font-mono">{row.provider} / {row.model}</td><td className="p-2">{display(row.avg_ttft_ms, row.ttft_sample_count, 'ms')} <span className="block text-muted-foreground">{row.ttft_sample_count} measured</span></td><td className="p-2">{display(row.avg_tokens_per_sec, row.tps_sample_count, 'tok/s')} <span className="block text-muted-foreground">{row.tps_sample_count} measured</span></td><td className="p-2">{display(row.avg_e2e_tokens_per_sec, row.e2e_sample_count, 'tok/s')} <span className="block text-muted-foreground">{row.e2e_sample_count} measured</span></td><td className="p-2">{row.sample_count}</td></tr>)}</tbody></table></div>}
  </CardContent></Card>;
}
