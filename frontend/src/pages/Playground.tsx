import React, { useEffect, useMemo, useState } from 'react';
import { ArrowLeft, Check, ChevronDown, Clipboard, Code2, Play, RotateCcw, Sparkles, TriangleAlert } from 'lucide-react';
import { Link } from 'react-router-dom';
import { Navigation } from '../components/layout/Navigation';
import { Button } from '../components/ui/Button';

type Model = { id: string; owned_by?: string };
type ChatMessage = { role: 'user' | 'assistant'; content: string };

const initialPrompt = 'Explain what makes Trefoil useful for routing AI requests in one short paragraph.';

export const Playground: React.FC = () => {
  const [models, setModels] = useState<Model[]>([]);
  const [model, setModel] = useState('');
  const [apiKey, setApiKey] = useState(() => localStorage.getItem('trefoil_playground_key') ?? '');
  const [prompt, setPrompt] = useState(initialPrompt);
  const [temperature, setTemperature] = useState('0.7');
  const [maxTokens, setMaxTokens] = useState('512');
  const [answer, setAnswer] = useState<ChatMessage | null>(null);
  const [raw, setRaw] = useState('');
  const [loading, setLoading] = useState(false);
  const [loadingModels, setLoadingModels] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    fetch('/v1/models')
      .then(async response => { if (!response.ok) throw new Error('Unable to load available models'); return response.json(); })
      .then(body => {
        const available = Array.isArray(body?.data) ? body.data.filter((item: Model) => item?.id) : [];
        setModels(available);
        if (available.length) setModel(available[0].id);
      })
      .catch(cause => setError(cause instanceof Error ? cause.message : 'Unable to load available models'))
      .finally(() => setLoadingModels(false));
  }, []);

  const request = useMemo(() => JSON.stringify({ model: model || 'your-model', messages: [{ role: 'user', content: prompt }], temperature: Number(temperature), max_tokens: Number(maxTokens) }, null, 2), [model, prompt, temperature, maxTokens]);

  const run = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!apiKey.trim()) { setError('Enter an inference API key to send a request.'); return; }
    if (!model) { setError('Choose a model before running the playground.'); return; }
    if (!prompt.trim()) { setError('Enter a prompt before running the playground.'); return; }
    setLoading(true); setError(null); setAnswer(null); setRaw('');
    localStorage.setItem('trefoil_playground_key', apiKey);
    try {
      const response = await fetch('/v1/chat/completions', { method: 'POST', headers: { 'content-type': 'application/json', 'x-api-key': apiKey.trim() }, body: request });
      const body = await response.json().catch(() => ({}));
      setRaw(JSON.stringify(body, null, 2));
      if (!response.ok) throw new Error(body?.error?.message || body?.error || `Request failed (${response.status})`);
      const message = body?.choices?.[0]?.message;
      setAnswer({ role: 'assistant', content: typeof message?.content === 'string' ? message.content : 'The model returned no text.' });
    } catch (cause) { setError(cause instanceof Error ? cause.message : 'Unable to complete request'); }
    finally { setLoading(false); }
  };

  const reset = () => { setPrompt(initialPrompt); setTemperature('0.7'); setMaxTokens('512'); setAnswer(null); setRaw(''); setError(null); };
  const copy = async () => { await navigator.clipboard.writeText(answer?.content || raw); setCopied(true); window.setTimeout(() => setCopied(false), 1500); };

  return <div className="min-h-screen bg-bg-deep text-text">
    <header className="border-b border-border bg-slate-950/70 px-4 py-4 sm:px-8"><div className="mx-auto flex max-w-6xl items-center justify-between gap-4">
      <div><p className="text-xs font-medium uppercase tracking-[0.2em] text-primary">Trefoil control plane</p><h1 className="mt-1 font-heading text-2xl font-semibold tracking-tight sm:text-3xl">Playground</h1><p className="mt-1 text-sm text-text-secondary">Send a request through your configured model routing</p></div>
      <Link to="/" className="inline-flex items-center gap-2 rounded-md border border-border-2 px-3 py-2 text-sm text-text-secondary hover:bg-slate-800 hover:text-text"><ArrowLeft size={15} /> <span className="hidden sm:inline">Overview</span></Link>
    </div></header>
    <main className="mx-auto max-w-6xl space-y-5 px-4 py-6 sm:px-8 sm:py-8"><Navigation />
      {error && <div role="alert" className="flex items-center gap-3 rounded-lg border border-red-500/30 bg-red-500/10 p-3 text-sm text-red-200"><TriangleAlert size={17} />{error}</div>}
      <form onSubmit={run} className="grid gap-5 lg:grid-cols-[minmax(0,1fr)_340px]">
        <section className="glass-bg rounded-xl p-5 sm:p-6"><div className="flex items-center justify-between gap-3"><div className="flex items-center gap-2"><Sparkles size={19} className="text-primary" /><h2 className="font-heading text-lg font-semibold">Try a prompt</h2></div><Button type="button" variant="ghost" size="sm" onClick={reset} leftIcon={<RotateCcw size={14} />}>Reset</Button></div>
          <label className="mt-5 block text-sm"><span className="mb-1.5 block text-text-secondary">Prompt</span><textarea value={prompt} onChange={event => setPrompt(event.target.value)} rows={8} className="w-full resize-y rounded-md border border-border-2 bg-slate-950/70 px-3 py-3 text-sm leading-6 outline-none focus:border-primary" placeholder="Ask your model anything…" /></label>
          <div className="mt-5 flex flex-wrap items-center justify-between gap-3"><span className="text-xs text-text-muted">Requests use the OpenAI-compatible chat completions API.</span><Button type="submit" size="lg" isLoading={loading} leftIcon={<Play size={15} />}>Run request</Button></div>
        </section>
        <aside className="glass-bg h-fit rounded-xl p-5 sm:p-6"><h2 className="font-heading text-lg font-semibold">Request settings</h2><div className="mt-5 space-y-4">
          <label className="block text-sm"><span className="mb-1.5 block text-text-secondary">Inference API key</span><input type="password" value={apiKey} onChange={event => setApiKey(event.target.value)} placeholder="sk-…" className="w-full rounded-md border border-border-2 bg-slate-950/70 px-3 py-2 font-mono text-sm outline-none focus:border-primary" /></label>
          <label className="block text-sm"><span className="mb-1.5 block text-text-secondary">Model</span><div className="relative"><select value={model} onChange={event => setModel(event.target.value)} disabled={loadingModels} className="w-full appearance-none rounded-md border border-border-2 bg-slate-950/70 px-3 py-2 pr-8 font-mono text-sm outline-none focus:border-primary"><option value="">{loadingModels ? 'Loading models…' : 'Select a model'}</option>{models.map(item => <option key={item.id} value={item.id}>{item.id}</option>)}</select><ChevronDown size={15} className="pointer-events-none absolute right-3 top-2.5 text-text-muted" /></div></label>
          <div className="grid grid-cols-2 gap-3"><label className="block text-sm"><span className="mb-1.5 block text-text-secondary">Temperature</span><input type="number" min="0" max="2" step="0.1" value={temperature} onChange={event => setTemperature(event.target.value)} className="w-full rounded-md border border-border-2 bg-slate-950/70 px-3 py-2 font-mono text-sm outline-none focus:border-primary" /></label><label className="block text-sm"><span className="mb-1.5 block text-text-secondary">Max tokens</span><input type="number" min="1" max="32768" step="1" value={maxTokens} onChange={event => setMaxTokens(event.target.value)} className="w-full rounded-md border border-border-2 bg-slate-950/70 px-3 py-2 font-mono text-sm outline-none focus:border-primary" /></label></div>
        </div></aside>
      </form>
      <section className="glass-bg rounded-xl p-5 sm:p-6"><div className="flex items-center justify-between gap-3"><div className="flex items-center gap-2"><Code2 size={18} className="text-primary" /><h2 className="font-heading text-lg font-semibold">Response</h2></div>{(answer || raw) && <Button type="button" variant="secondary" size="sm" onClick={() => void copy()} leftIcon={copied ? <Check size={14} /> : <Clipboard size={14} />}>{copied ? 'Copied' : 'Copy'}</Button>}</div>{answer ? <div className="mt-5 whitespace-pre-wrap rounded-lg border border-border bg-slate-950/60 p-4 text-sm leading-7">{answer.content}</div> : raw ? <pre className="mt-5 max-h-96 overflow-auto rounded-lg border border-border bg-slate-950/60 p-4 font-mono text-xs leading-6 text-text-secondary">{raw}</pre> : <div className="mt-5 rounded-lg border border-dashed border-border-2 py-12 text-center text-sm text-text-muted">Your model response will appear here.</div>}</section>
    </main>
  </div>;
};
