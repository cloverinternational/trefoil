import React, { useEffect, useMemo, useState } from 'react';
import { CheckCircle2, ChevronDown, Code2, Play, RefreshCw, Route, ShieldAlert, SlidersHorizontal, TriangleAlert } from 'lucide-react';
import { useAuth } from '../contexts/AuthContext';
import { getAliases, getKeys, getProviders, getRecentUsage, type ApiKeyConfig, type ModelAlias } from '../lib/api';
import { Button } from '../components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '../components/ui/card';
import { Input } from '../components/ui/input';
import { Label } from '../components/ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '../components/ui/select';

type Model = { id: string };
type RouteState = { status: 'idle' | 'pending' | 'complete' | 'error'; provider?: string; model?: string; requestId?: string };
type ApiMode = 'chat' | 'messages' | 'responses' | 'gemini';

export const Playground: React.FC = () => {
  const { adminKey, isAdmin } = useAuth();
  const [keys, setKeys] = useState<Record<string, ApiKeyConfig>>({});
  const [keyName, setKeyName] = useState('session');
  const [overrideKey, setOverrideKey] = useState('');
  const [models, setModels] = useState<Model[]>([]);
  const [model, setModel] = useState('');
  const [directModel, setDirectModel] = useState('');
  const [prompt, setPrompt] = useState('');
  const [temperature, setTemperature] = useState('');
  const [maxTokens, setMaxTokens] = useState('128');
  const [apiMode, setApiMode] = useState<ApiMode>('chat');
  const [advanced, setAdvanced] = useState(false);
  const [loading, setLoading] = useState(true);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [answer, setAnswer] = useState('');
  const [raw, setRaw] = useState('');
  const [route, setRoute] = useState<RouteState>({ status: 'idle' });

  const credential = overrideKey.trim() || (keyName === 'session' ? adminKey?.trim() : keys[keyName]?.secret?.trim()) || '';
  const selectedModel = directModel.trim() || model;
  const selectedKey = keys[keyName];
  const policyBlocked = !!selectedKey && !!selectedModel && (
    (!!selectedKey.allowedModels?.length && !selectedKey.allowedModels.includes(selectedModel)) ||
    !!selectedKey.excludedModels?.includes(selectedModel)
  );
  const payload = useMemo(() => ({ model: selectedModel, messages: [{ role: 'user', content: prompt.trim() }], ...(temperature === '' ? {} : { temperature: Number(temperature) }), max_tokens: Number(maxTokens) }), [selectedModel, prompt, temperature, maxTokens]);

  useEffect(() => {
    let active = true;
    if (!credential) { setModels([]); setLoading(false); return; }
    setLoading(true);
    void (async () => {
      const response = await fetch('/v1/models', { headers: { 'x-api-key': credential } });
      if (!response.ok) throw new Error('Unable to load models for this key');
      const body = await response.json();
      const advertised: Model[] = Array.isArray(body?.data) ? body.data.filter((item: Model) => typeof item?.id === 'string' && !!item.id) : [];
      if (!isAdmin || overrideKey.trim() || keyName !== 'session') return advertised;
      const [aliases, providers, configuredKeys] = await Promise.all([getAliases(), getProviders(), getKeys()]);
      if (active) setKeys(configuredKeys);
      const routable = (slug: string, seen = new Set<string>()): boolean => {
        if (seen.has(slug)) return false;
        const alias: ModelAlias | undefined = aliases[slug];
        if (!alias) return false;
        seen.add(slug);
        return (alias.target_groups ?? []).some(group => group.targets?.some(target => target.enabled !== false && (
          (!!target.provider && !!target.model && !!providers[target.provider]?.enabled) ||
          (!!target.alias && routable(target.alias, new Set(seen)))
        )));
      };
      const usable = advertised.filter(item => routable(item.id) && !providers[item.id]);
      for (const [slug, provider] of Object.entries(providers)) {
        if (!provider.enabled) continue;
        for (const group of Object.values(aliases)) for (const targets of group.target_groups ?? []) for (const target of targets.targets ?? []) {
          if (target.enabled !== false && target.provider === slug && target.model) {
            const id = `${slug}/${target.model}`;
            if (!usable.some(item => item.id === id)) usable.push({ id });
          }
        }
      }
      return usable.sort((a, b) => a.id.localeCompare(b.id));
    })().then(available => { if (!active) return; setModels(available); setModel(previous => available.some(item => item.id === previous) ? previous : available[0]?.id ?? ''); setError(null); })
      .catch(cause => { if (active) { setModels([]); setModel(''); setError(cause instanceof Error ? cause.message : 'Unable to load models'); } })
      .finally(() => { if (active) setLoading(false); });
    return () => { active = false; };
  }, [credential, isAdmin, keyName, overrideKey]);

  const refresh = () => { setError(null); setKeyName('session'); setOverrideKey(''); setModel(''); setRoute({ status: 'idle' }); window.location.reload(); };
  const send = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!credential) { setError('Choose a client key or use the signed-in session.'); return; }
    if (!selectedModel) { setError('Choose a model before sending.'); return; }
    if (directModel && !/^[^\s/]+\/[^\s/]+$/.test(directModel.trim())) { setError('Direct model must use provider/model format.'); return; }
    if (!prompt.trim()) { setError('Enter a prompt before sending.'); return; }
    if (temperature !== '' && (!Number.isFinite(Number(temperature)) || Number(temperature) < 0 || Number(temperature) > 2)) { setError('Temperature must be blank or between 0 and 2.'); return; }
    if (!/^\d+$/.test(maxTokens) || Number(maxTokens) < 1 || Number(maxTokens) > 32768) { setError('Max tokens must be a whole number from 1 to 32768.'); return; }
    setRunning(true); setError(null); setAnswer(''); setRaw(''); setRoute({ status: 'pending' });
    try {
      const paths: Record<ApiMode, string> = { chat: '/v1/chat/completions', messages: '/v1/messages', responses: '/v1/responses', gemini: `/v1beta/models/${encodeURIComponent(selectedModel)}:generateContent` };
      const requestBody = apiMode === 'messages'
        ? { model: selectedModel, messages: payload.messages, max_tokens: payload.max_tokens, ...(temperature === '' ? {} : { temperature: Number(temperature) }) }
        : apiMode === 'responses'
          ? { model: selectedModel, input: prompt.trim(), max_output_tokens: payload.max_tokens, ...(temperature === '' ? {} : { temperature: Number(temperature) }) }
          : apiMode === 'gemini'
            ? { contents: [{ role: 'user', parts: [{ text: prompt.trim() }] }], generationConfig: { maxOutputTokens: payload.max_tokens, ...(temperature === '' ? {} : { temperature: Number(temperature) }) } }
            : payload;
      // Trefoil authenticates the client using x-api-key; Gemini providers
      // additionally expect x-goog-api-key on their upstream wire request.
      const response = await fetch(paths[apiMode], { method: 'POST', headers: { 'content-type': 'application/json', 'x-api-key': credential, ...(apiMode === 'gemini' ? { 'x-goog-api-key': credential } : {}), ...(apiMode === 'messages' ? { 'anthropic-version': '2023-06-01' } : {}) }, body: JSON.stringify(requestBody) });
      const body = await response.json().catch(() => ({}));
      setRaw(JSON.stringify(body, null, 2));
      const requestId = response.headers.get('x-request-id') ?? body?.id ?? undefined;
      if (!response.ok) throw new Error(body?.error?.message || body?.error || `Request failed (${response.status})`);
      const content = apiMode === 'messages'
        ? (Array.isArray(body?.content) ? body.content.filter((part: { type?: string; text?: unknown }) => part?.type === 'text' && typeof part.text === 'string').map((part: { text: string }) => part.text).join('\n') : '')
        : apiMode === 'responses'
          ? (typeof body?.output_text === 'string' ? body.output_text : Array.isArray(body?.output) ? body.output.flatMap((item: { content?: { type?: string; text?: unknown }[] }) => Array.isArray(item.content) ? item.content.filter(part => part?.type === 'output_text' && typeof part.text === 'string').map(part => part.text as string) : []).join('\n') : '')
          : apiMode === 'gemini'
            ? (Array.isArray(body?.candidates?.[0]?.content?.parts) ? body.candidates[0].content.parts.filter((part: { text?: unknown }) => typeof part.text === 'string').map((part: { text: string }) => part.text).join('\n') : '')
            : body?.choices?.[0]?.message?.content;
      if (typeof content !== 'string' || !content.trim()) throw new Error('The request succeeded but returned no assistant text. Inspect the raw response.');
      setAnswer(content);
      const routeInfo: RouteState = { status: 'complete', requestId, model: typeof body?.model === 'string' ? body.model : undefined };
      if (isAdmin && requestId) {
        try {
          const rows = await getRecentUsage(20);
          const match = rows.find(row => row.requestId === requestId);
          if (match) routeInfo.provider = match.provider ?? undefined;
        } catch { /* Routing metadata is optional; the response remains valid. */ }
      }
      setRoute(routeInfo);
    } catch (cause) { setError(cause instanceof Error ? cause.message : 'Request failed'); setRoute({ status: 'error' }); }
    finally { setRunning(false); }
  };

  return <main className="mx-auto w-full max-w-7xl space-y-5 p-4 sm:p-6 lg:p-8">
    <header className="flex flex-wrap items-center justify-between gap-3 border-b border-border pb-4">
      <div><h1 className="font-heading text-2xl font-semibold">Playground</h1><p className="mt-1 text-sm text-muted-foreground">Simulate client keys to test quotas, routing, and model access policies.</p></div>
      <Button type="button" variant="outline" size="sm" onClick={refresh} disabled={loading || running}><RefreshCw size={15}/>Refresh</Button>
    </header>
    {error && <div role="alert" className="flex items-start gap-2 rounded-md border border-destructive/40 bg-destructive/10 p-3 text-sm text-destructive"><TriangleAlert size={17}/>{error}</div>}
    <Card><CardHeader className="border-b border-border pb-3"><CardTitle className="text-base">Test Configuration</CardTitle></CardHeader><CardContent className="grid gap-4 pt-4 sm:grid-cols-[minmax(14rem,1fr)_minmax(0,2fr)]">
      <div className="space-y-3">
        <div className="space-y-1"><Label htmlFor="playground-key" className="text-xs uppercase tracking-wide">Key</Label><Select value={keyName} onValueChange={setKeyName}><SelectTrigger id="playground-key" aria-label="Key"><SelectValue/></SelectTrigger><SelectContent><SelectItem value="session">Signed-in session</SelectItem>{isAdmin && Object.keys(keys).sort().map(name => <SelectItem key={name} value={name}>{name}</SelectItem>)}</SelectContent></Select></div>
        <div className="space-y-1"><Label htmlFor="playground-model" className="text-xs uppercase tracking-wide">Model</Label><Select value={model} onValueChange={setModel} disabled={loading || !models.length}><SelectTrigger id="playground-model" aria-label="Model"><SelectValue placeholder={loading ? 'Loading models…' : 'Select a model'}/></SelectTrigger><SelectContent>{models.map(item => <SelectItem key={item.id} value={item.id}>{item.id}</SelectItem>)}</SelectContent></Select></div>
        <div className="space-y-1"><Label className="text-xs uppercase tracking-wide">API</Label><Select value={apiMode} onValueChange={value => setApiMode(value as ApiMode)}><SelectTrigger aria-label="API"><SelectValue/></SelectTrigger><SelectContent><SelectItem value="chat">chat</SelectItem><SelectItem value="messages">messages</SelectItem><SelectItem value="responses">responses</SelectItem><SelectItem value="gemini">Gemini</SelectItem></SelectContent></Select></div>
        <label className="flex items-center gap-2 text-xs text-muted-foreground"><input type="checkbox" disabled aria-label="Sample browser tools"/>Sample browser tools <span>(not available)</span></label>
      </div>
      <div className="min-w-0 space-y-3 border-t border-border pt-3 text-xs sm:border-l sm:border-t-0 sm:pl-4 sm:pt-0">
        <div className="grid grid-cols-[5rem_minmax(0,1fr)] gap-2"><span className="uppercase text-muted-foreground">Policy</span><span className={policyBlocked ? 'text-destructive' : 'text-success'}>{policyBlocked ? 'Model blocked by key policy' : selectedKey ? 'Model allowed by key policy' : 'Signed-in session'}</span></div>
        <div className="grid grid-cols-[5rem_minmax(0,1fr)] gap-2"><span className="uppercase text-muted-foreground">Comment</span><span className="truncate">{selectedKey?.comment || '—'}</span></div>
        <div className="grid grid-cols-[5rem_minmax(0,1fr)] gap-2"><span className="uppercase text-muted-foreground">Models</span><span className="truncate">Allow {selectedKey?.allowedModels?.join(', ') || 'All'} · exclude {selectedKey?.excludedModels?.join(', ') || 'None'}</span></div>
        <div className="grid grid-cols-[5rem_minmax(0,1fr)] gap-2"><span className="uppercase text-muted-foreground">Providers</span><span className="truncate">Allow {selectedKey?.allowedProviders?.join(', ') || 'All'} · exclude {selectedKey?.excludedProviders?.join(', ') || 'None'}</span></div>
        <div className="grid grid-cols-[5rem_minmax(0,1fr)] gap-2"><span className="uppercase text-muted-foreground">Access</span><span className="truncate">IPs {selectedKey?.allowedIps?.join(', ') || 'Any'} · quotas {selectedKey?.quotas?.join(', ') || 'Defaults'}</span></div>
        <details className="border-t border-border pt-2"><summary className="flex cursor-pointer items-center gap-1 text-sm font-medium"><ChevronDown size={14}/>Advanced request settings</summary><div className="mt-3 space-y-3">
          <div className="space-y-1"><Label htmlFor="apiKey">Inference key override (optional)</Label><Input id="apiKey" type="password" autoComplete="off" value={overrideKey} onChange={event => setOverrideKey(event.target.value)} placeholder="Use selected key"/><p className="text-muted-foreground">Not saved; overrides the selected key for this request.</p></div>
          <div className="space-y-1"><Label htmlFor="customModel">Direct provider/model (optional)</Label><Input id="customModel" value={directModel} onChange={event => setDirectModel(event.target.value)} placeholder="provider/model"/></div>
          <div className="grid grid-cols-2 gap-3"><div><Label htmlFor="temperature">Temperature (optional)</Label><Input id="temperature" type="number" min="0" max="2" step="0.1" value={temperature} onChange={event => setTemperature(event.target.value)} placeholder="Provider default"/></div><div><Label htmlFor="maxTokens">Max tokens</Label><Input id="maxTokens" type="number" min="1" max="32768" step="1" value={maxTokens} onChange={event => setMaxTokens(event.target.value)}/></div></div>
        </div></details>
      </div>
    </CardContent></Card>
    <div className="grid gap-4 xl:grid-cols-[minmax(0,1fr)_22rem]">
      <Card className="min-w-0"><CardHeader className="flex-row items-center justify-between border-b border-border"><CardTitle className="text-base">Chat Simulation</CardTitle><span className="flex min-w-0 items-center gap-1 truncate text-xs text-muted-foreground"><SlidersHorizontal size={14}/>{apiMode} · {selectedModel || 'No model selected'}</span></CardHeader>
        <CardContent className="p-3 sm:p-4"><div className="flex h-[clamp(18rem,calc(100dvh-22rem),42.5rem)] flex-col overflow-hidden rounded-lg border border-border bg-background">
          <div className="flex-1 space-y-4 overflow-y-auto p-4">{!answer && !running && !raw ? <div className="flex h-full flex-col items-center justify-center text-center text-sm text-muted-foreground"><Route className="mb-3 text-primary" size={26}/><strong className="text-foreground">Trefoil simulation is active</strong><span>Model “{selectedModel || 'none'}”</span></div> : <><div className="ml-auto max-w-[90%] whitespace-pre-wrap rounded-lg bg-primary/10 p-3 text-sm"><p className="mb-1 text-xs text-muted-foreground">You</p>{prompt}</div>{answer && <div className="max-w-[90%] whitespace-pre-wrap rounded-lg bg-muted p-3 text-sm"><p className="mb-1 text-xs text-muted-foreground">Assistant</p>{answer}</div>}</>}</div>
          <form onSubmit={send} className="flex gap-2 border-t border-border p-3"><textarea aria-label="Message" value={prompt} onChange={event => setPrompt(event.target.value)} rows={2} placeholder="Send a test prompt through Trefoil…" className="min-w-0 flex-1 resize-none rounded-md border border-input bg-transparent px-3 py-2 text-sm"/><Button type="submit" aria-label="Run request" disabled={running || loading} className="self-end"><Play size={17}/>{running ? 'Running…' : 'Send'}</Button></form>
        </div></CardContent>
      </Card>
      <Card className="min-w-0"><CardHeader className="flex-row items-center justify-between border-b border-border"><CardTitle className="text-base">Routing Decision</CardTitle><span role="status" className="text-xs text-muted-foreground">{route.status === 'idle' ? 'Waiting' : route.status === 'pending' ? 'Resolving' : route.status === 'complete' ? 'Routed' : 'Failed'}</span></CardHeader><CardContent className="space-y-3 pt-4 text-sm">
        <div className="rounded-md border border-border p-3"><p className="text-xs uppercase text-muted-foreground">Observed route</p><p className="mt-1 font-medium">{route.provider ? `${route.provider} / ${route.model || 'model unavailable'}` : route.status === 'complete' ? 'Request completed (provider unavailable)' : route.status === 'error' ? 'Request failed' : 'Pending'}</p>{route.provider && <p className="mt-1 text-xs text-muted-foreground">Trefoil provider / response model; upstream provider is not verified here.</p>}</div>
        <div className="grid grid-cols-2 gap-2"><div className="rounded-md bg-muted/40 p-3"><p className="text-xs uppercase text-muted-foreground">Alias</p>{selectedModel || '—'}</div><div className="rounded-md bg-muted/40 p-3"><p className="text-xs uppercase text-muted-foreground">API</p>{apiMode}</div></div>
        <div className="rounded-md border border-border p-3"><p className="text-xs uppercase text-muted-foreground">Decision trail</p>{route.requestId ? <p className="mt-1 break-all font-mono text-xs">Request ID: {route.requestId}</p> : <p className="mt-1 text-xs text-muted-foreground">Routing details appear after a request.</p>}</div>
        {raw && <details><summary className="flex cursor-pointer items-center gap-2 text-xs text-muted-foreground"><Code2 size={14}/>Raw response</summary><pre className="mt-2 max-h-64 overflow-auto whitespace-pre-wrap rounded-md border border-border p-2 text-xs">{raw}</pre></details>}
      </CardContent></Card>
    </div>
  </main>;
};
