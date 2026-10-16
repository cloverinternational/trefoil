import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { AlertTriangle, Check, Plus, RefreshCw, Settings2, Trash2 } from 'lucide-react';
import { getSystemSettings, patchSystemSettings, type SystemSettings } from '../lib/api';
import { Button } from '../components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '../components/ui/card';
import { Input } from '../components/ui/input';
import { Label } from '../components/ui/label';

type Entry = { key: string; value: string };
const stringify = (value: unknown) => JSON.stringify(value, null, 2);

export const Configuration: React.FC = () => {
  const [entries, setEntries] = useState<Entry[]>([]);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const load = useCallback(async () => {
    setLoading(true); setError(null); setSaved(false);
    try { const settings = await getSystemSettings(); setEntries(Object.entries(settings).sort(([a], [b]) => a.localeCompare(b)).map(([key, value]) => ({ key, value: stringify(value) }))); }
    catch (cause) { setError(cause instanceof Error ? cause.message : 'Unable to load system settings'); }
    finally { setLoading(false); }
  }, []);
  useEffect(() => { void load(); }, [load]);
  const update = (index: number, field: keyof Entry, value: string) => setEntries(items => items.map((item, i) => i === index ? { ...item, [field]: value } : item));
  const hasDuplicate = useMemo(() => entries.some((entry, index) => entry.key.trim() && entries.findIndex(other => other.key.trim() === entry.key.trim()) !== index), [entries]);
  const save = async (event: React.FormEvent) => {
    event.preventDefault(); setError(null); setSaved(false);
    const settings: SystemSettings = {};
    for (const entry of entries) {
      const key = entry.key.trim();
      if (!key) { setError('Every setting must have a name.'); return; }
      if (!/^[-a-zA-Z0-9_.]+$/.test(key)) { setError(`Invalid setting name “${key}”.`); return; }
      if (hasDuplicate) { setError('Setting names must be unique.'); return; }
      try { settings[key] = JSON.parse(entry.value); } catch { setError(`Value for “${key}” must be valid JSON.`); return; }
    }
    setSaving(true);
    try { await patchSystemSettings(settings); setSaved(true); await load(); }
    catch (cause) { setError(cause instanceof Error ? cause.message : 'Unable to save system settings'); }
    finally { setSaving(false); }
  };

  return <main className="mx-auto w-full max-w-5xl space-y-6 p-4 sm:p-8">
    <header className="flex flex-wrap items-start justify-between gap-4"><div><p className="text-sm font-medium text-primary">Gateway administration</p><h1 className="mt-1 font-heading text-3xl font-semibold">System configuration</h1><p className="mt-2 text-muted-foreground">Manage runtime settings for the gateway.</p></div><div className="flex gap-2"><Button variant="outline" onClick={() => void load()} disabled={loading || saving}><RefreshCw className={loading ? 'animate-spin' : ''} />Refresh</Button><Button onClick={() => setEntries(items => [...items, { key: '', value: 'true' }])} disabled={loading || saving}><Plus />Add setting</Button></div></header>
    {error && <div role="alert" className="flex items-center gap-3 rounded-md border border-destructive/30 bg-destructive/10 p-3 text-sm text-destructive"><AlertTriangle className="size-4" />{error}</div>}
    {saved && <div role="status" className="flex items-center gap-3 rounded-md border border-emerald-500/30 bg-emerald-500/10 p-3 text-sm text-emerald-700"><Check className="size-4" />System settings saved.</div>}
    <Card><CardHeader><div className="flex items-center gap-2"><Settings2 className="size-5 text-primary" /><CardTitle>Runtime settings</CardTitle></div><CardDescription>Values are entered as JSON. Strings need quotes; booleans, numbers, arrays, and objects are supported.</CardDescription></CardHeader>
      {loading ? <CardContent><p className="py-12 text-center text-sm text-muted-foreground">Loading system settings…</p></CardContent> : <form onSubmit={save}><CardContent className="space-y-4">
        {entries.length === 0 ? <div className="py-10 text-center"><Settings2 className="mx-auto size-7 text-muted-foreground" /><p className="mt-3 font-medium">No system settings configured</p><p className="mt-1 text-sm text-muted-foreground">Add a setting to customize gateway behavior.</p></div> : entries.map((entry, index) => <div key={index} className="rounded-lg border border-border p-4"><div className="flex items-end gap-3"><div className="min-w-0 flex-1 space-y-2"><Label htmlFor={`setting-name-${index}`}>Setting name</Label><Input id={`setting-name-${index}`} required value={entry.key} onChange={event => update(index, 'key', event.target.value)} className="font-mono" placeholder="feature.example" /></div><Button type="button" variant="ghost" size="icon" aria-label={`Remove ${entry.key || 'setting'}`} onClick={() => setEntries(items => items.filter((_, i) => i !== index))}><Trash2 /></Button></div><div className="mt-4 space-y-2"><Label htmlFor={`setting-value-${index}`}>Value (JSON)</Label><textarea id={`setting-value-${index}`} required rows={Math.max(2, Math.min(6, entry.value.split('\n').length))} value={entry.value} onChange={event => update(index, 'value', event.target.value)} className="flex w-full resize-y rounded-md border border-input bg-transparent px-3 py-2 font-mono text-sm shadow-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring" spellCheck={false} /></div></div>)}
      </CardContent><div className="flex justify-end border-t border-border p-6"><Button type="submit" disabled={loading || saving || hasDuplicate}>{saving ? 'Saving…' : 'Save settings'}</Button></div></form>}
    </Card>
  </main>;
};
