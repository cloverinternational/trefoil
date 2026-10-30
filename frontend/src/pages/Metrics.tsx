import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { Activity, AlertTriangle, BarChart3, RefreshCw, Server } from 'lucide-react';
import ReactECharts from 'echarts-for-react';
import { useTheme } from '../contexts/ThemeContext';
import { useAuth } from '../contexts/AuthContext';
import { getMetrics, getUsageSeries, type UsageBucket } from '../lib/api';
import { Button } from '../components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '../components/ui/card';
import { Skeleton } from '../components/ui/skeleton';
import { bucketLabel } from '../lib/utils';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '../components/ui/select';

type Metric = { name: string; labels: Record<string, string>; value: number };
function parseMetrics(text: string): Metric[] { const result: Metric[]=[]; for (const line of text.split('\n')) { const match=line.match(/^([a-zA-Z_:][a-zA-Z0-9_:]*)(?:\{([^}]*)\})?\s+([-+0-9.eE]+)$/); if (!match) continue; const labels:Record<string,string>={}; for (const item of (match[2]??'').matchAll(/([a-zA-Z_][a-zA-Z0-9_]*)="((?:\\.|[^"\\])*)"/g)) labels[item[1]]=item[2]; const value=Number(match[3]); if(Number.isFinite(value)) result.push({name:match[1],labels,value}); } return result; }
const valueOf=(metrics:Metric[],name:string)=>metrics.find(metric=>metric.name===name)?.value??0;
const fmt=new Intl.NumberFormat();
export const Metrics: React.FC = () => {
  const { theme } = useTheme();
  const chart = { green: theme === "dark" ? "#60d291" : "#187647", neutral: theme === "dark" ? "#a3a3a3" : "#737373", label: theme === "dark" ? "#a3a3a3" : "#737373", grid: theme === "dark" ? "#262626" : "#e5e5e5" };
  const {isAdmin}=useAuth(); const [raw,setRaw]=useState(''); const [loading,setLoading]=useState(true); const [error,setError]=useState<string|null>(null);
  const [series,setSeries]=useState<UsageBucket[]>([]); const [range,setRange]=useState<'day'|'week'|'month'>('day');
  const load=useCallback(async()=>{setLoading(true);setError(null);try{const [text,buckets]=await Promise.all([getMetrics(),getUsageSeries(range)]);setRaw(text);setSeries(buckets);}catch(cause){setError(cause instanceof Error?cause.message:'Unable to load metrics');}finally{setLoading(false);}},[range]);
  useEffect(()=>{if(isAdmin) void load(); else setLoading(false);},[isAdmin,load]);
  const metrics=useMemo(()=>parseMetrics(raw),[raw]); const providers=metrics.filter(metric=>metric.name==='plexus_provider_requests_total');
  const values=[valueOf(metrics,'plexus_requests_total'),valueOf(metrics,'plexus_errors_total'),valueOf(metrics,'plexus_requests_today'),valueOf(metrics,'plexus_avg_duration_ms')];
  const activityOption = React.useMemo(() => ({
    backgroundColor: 'transparent',
    tooltip: { trigger: 'axis' },
    legend: { data: ['Requests', 'Errors'], textStyle: { color: chart.label } },
    grid: { left: 48, right: 20, top: 30, bottom: 28 },
    xAxis: { type: 'category', boundaryGap: false,
      data: series.map(b => bucketLabel(b.bucketStartMs, range)),
      axisLabel: { color: chart.label }, axisLine: { lineStyle: { color: chart.grid } } },
    yAxis: { type: 'value', axisLabel: { color: chart.label }, splitLine: { lineStyle: { color: chart.grid } } },
    series: [
      { name: 'Requests', type: 'line', smooth: true, data: series.map(b => b.requests), color: chart.green, areaStyle: { opacity: 0.25 } },
      { name: 'Errors', type: 'line', smooth: true, data: series.map(b => b.errors), color: '#ef4444' },
    ],
  }), [series, range, theme]);
  return <div className="space-y-3"><div className="flex items-start justify-between"><div><h1 className="display">Metrics</h1><p className="mt-1 text-muted-foreground">Gateway performance and provider activity</p></div>{isAdmin&&<Button variant="outline" onClick={()=>void load()} disabled={loading}><RefreshCw className={loading?'animate-spin':''}/>Refresh</Button>}</div>
    {!isAdmin?<Card><CardContent className="py-6 text-center"><AlertTriangle className="mx-auto text-primary"/><h2 className="mt-4 font-heading text-base font-semibold">Administrator access required</h2><p className="mt-2 text-muted-foreground">Operational metrics are available only to administrators.</p></CardContent></Card>:<>{error&&<div role="alert" className=" border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">{error}</div>}{loading?<div className="grid grid-cols-2 gap-3 md:grid-cols-4">{[1,2,3,4].map(i=><Skeleton key={i} className="h-16"/>)}<Skeleton className="col-span-2 h-56 md:col-span-4"/></div>:<><div className="grid grid-cols-2 gap-3 md:grid-cols-4">{[['Requests',values[0],Activity],['Errors',values[1],AlertTriangle],['Requests today',values[2],BarChart3],['Avg. duration',values[3],Server]].map(([label,value,Icon])=>{const MetricIcon=Icon as React.ElementType; return <Card key={label as string}><CardContent className="p-2"><div className="flex justify-between text-xs text-muted-foreground">{label as string}<MetricIcon size={13}/></div><p className="mt-1 stat-num text-lg font-semibold">{fmt.format(value as number)}{label==='Avg. duration'?' ms':''}</p></CardContent></Card>;})}</div><Card><CardHeader className="flex-row items-center justify-between"><CardTitle>Gateway activity</CardTitle><Select value={range} onValueChange={v=>setRange(v as 'day'|'week'|'month')}><SelectTrigger className="w-32"><SelectValue/></SelectTrigger><SelectContent><SelectItem value="day">Day</SelectItem><SelectItem value="week">Week</SelectItem><SelectItem value="month">Month</SelectItem></SelectContent></Select></CardHeader><CardContent>{series.length===0?<p className="py-24 text-center text-xs text-muted-foreground">No activity recorded yet. Requests will appear here after the gateway handles traffic.</p>:<ReactECharts style={{height:290}} option={activityOption}/>}</CardContent></Card><Card><CardHeader><CardTitle>Provider activity</CardTitle></CardHeader><CardContent>{providers.length===0?<p className="py-8 text-center text-xs text-muted-foreground">No provider activity recorded yet.</p>:<ReactECharts style={{height:280}} option={{backgroundColor:'transparent',tooltip:{trigger:'axis'},grid:{left:48,right:20,top:20,bottom:55},xAxis:{type:'category',data:providers.map(x=>x.labels.provider||'unknown'),axisLabel:{color:chart.label,rotate:25}},yAxis:{type:'value',axisLabel:{color:chart.label},splitLine:{lineStyle:{color:chart.grid}}},series:[{type:'bar',data:providers.map(x=>x.value),itemStyle:{color:chart.neutral}}]}}/>}</CardContent></Card></>}</>}
  </div>;
};
