import React, { useCallback, useEffect, useState } from 'react';
import { AlertTriangle, ArrowLeft, Boxes, Pencil, Plus, RefreshCw, Trash2 } from 'lucide-react';
import { Link } from 'react-router-dom';
import { Navigation } from '../components/layout/Navigation';
import { Button } from '../components/ui/Button';
import { deleteAlias, getAliases, saveAlias, type ModelAlias } from '../lib/api';

type FormState = { slug: string; target: string; provider: string; selector: string; additional: string };
const blank: FormState = { slug: '', target: '', provider: '', selector: 'random', additional: '' };
const slugPattern = /^[a-z0-9][a-z0-9._-]{0,62}$/;

export const ModelAliases: React.FC = () => {
  const [aliases, setAliases] = useState<Record<string, ModelAlias>>({});
  const [form, setForm] = useState<FormState | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const load = useCallback(async () => { setLoading(true); setError(null); try { setAliases(await getAliases()); } catch (e) { setError(e instanceof Error ? e.message : 'Unable to load model aliases'); } finally { setLoading(false); } }, []);
  useEffect(() => { void load(); }, [load]);

  const edit = (slug: string, alias?: ModelAlias) => {
    const target = alias?.target_groups?.[0]?.targets?.[0];
    setForm({ slug, target: target?.model ?? target?.alias ?? '', provider: target?.provider ?? '', selector: alias?.target_groups?.[0]?.selector ?? 'random', additional: alias?.additional_aliases?.join(', ') ?? '' });
    setError(null);
  };
  const submit = async (event: React.FormEvent) => {
    event.preventDefault(); if (!form) return;
    const slug = form.slug.trim(); const target = form.target.trim(); const provider = form.provider.trim();
    if (!slugPattern.test(slug)) { setError('Alias ID must use lowercase letters, numbers, dots, underscores, or hyphens.'); return; }
    if (!target || !provider) { setError('A provider and model are required.'); return; }
    const additional = [...new Set(form.additional.split(',').map(value => value.trim()).filter(Boolean))];
    setSaving(true); setError(null);
    try {
      await saveAlias(slug, { priority: 'selector', target_groups: [{ name: 'default', selector: form.selector, targets: [{ provider, model: target, enabled: true }] }], additional_aliases: additional }, !!aliases[slug]);
      setForm(null); await load();
    } catch (e) { setError(e instanceof Error ? e.message : 'Unable to save model alias'); } finally { setSaving(false); }
  };
  const remove = async (slug: string) => { if (!window.confirm(`Delete model alias “${slug}”?`)) return; setError(null); try { await deleteAlias(slug); await load(); } catch (e) { setError(e instanceof Error ? e.message : 'Unable to delete model alias'); } };

  return <div className="min-h-screen bg-bg-deep text-text"><header className="border-b border-border bg-slate-950/70 px-4 py-4 sm:px-8"><div className="mx-auto flex max-w-6xl items-center justify-between"><div><p className="text-xs font-medium uppercase tracking-[0.2em] text-primary">Trefoil control plane</p><h1 className="mt-1 font-heading text-2xl font-semibold">Model aliases</h1><p className="mt-1 text-sm text-text-secondary">Route friendly model names to configured providers</p></div><div className="flex gap-2"><Button variant="secondary" size="sm" onClick={() => void load()} disabled={loading} leftIcon={<RefreshCw size={14} className={loading ? 'animate-spin' : ''} />}>Refresh</Button><Button size="sm" onClick={() => edit('')} leftIcon={<Plus size={14} />}>Add alias</Button></div></div></header>
    <main className="mx-auto max-w-6xl space-y-5 px-4 py-6 sm:px-8 sm:py-8"><Navigation /><Link to="/" className="inline-flex items-center gap-2 text-sm text-text-secondary hover:text-text"><ArrowLeft size={15} /> Back to overview</Link>{error && <div role="alert" className="flex items-center gap-3 rounded-lg border border-red-500/30 bg-red-500/10 p-3 text-sm text-red-200"><AlertTriangle size={17} />{error}</div>}
      {loading ? <div className="glass-bg rounded-xl py-16 text-center text-sm text-text-muted">Loading model aliases…</div> : Object.keys(aliases).length === 0 ? <div className="glass-bg rounded-xl py-16 text-center"><Boxes size={26} className="mx-auto text-text-muted" /><p className="mt-3 font-medium">No model aliases configured</p><p className="mt-1 text-sm text-text-secondary">Add an alias to expose a stable model name.</p></div> : <div className="grid gap-4 sm:grid-cols-2">{Object.entries(aliases).sort(([a], [b]) => a.localeCompare(b)).map(([slug, alias]) => { const target = alias.target_groups?.[0]?.targets?.[0]; return <article key={slug} className="glass-bg rounded-xl p-5"><div className="flex items-start justify-between gap-3"><div><h2 className="font-heading text-lg font-semibold">{slug}</h2><p className="mt-1 font-mono text-xs text-text-muted">{target?.provider && target?.model ? `${target.provider} / ${target.model}` : target?.alias || 'No target configured'}</p></div><span className="rounded-full bg-primary/10 px-2 py-1 text-xs text-primary">{alias.target_groups?.length ?? 0} target group{alias.target_groups?.length === 1 ? '' : 's'}</span></div>{alias.additional_aliases?.length ? <p className="mt-4 text-xs text-text-secondary">Also known as {alias.additional_aliases.join(', ')}</p> : null}<div className="mt-5 flex justify-end gap-2 border-t border-border pt-4"><Button variant="ghost" size="sm" onClick={() => edit(slug, alias)} leftIcon={<Pencil size={14} />}>Edit</Button><Button variant="danger" size="sm" onClick={() => void remove(slug)} leftIcon={<Trash2 size={14} />}>Delete</Button></div></article>; })}</div>}
      {form && <div className="fixed inset-0 z-10 flex items-center justify-center bg-black/70 p-4" role="dialog" aria-modal="true" aria-labelledby="alias-form-title"><form onSubmit={submit} className="glass-bg w-full max-w-lg rounded-xl p-5 shadow-2xl sm:p-6"><h2 id="alias-form-title" className="font-heading text-xl font-semibold">{form.slug ? 'Edit model alias' : 'Add model alias'}</h2><div className="mt-5 space-y-4"><label className="block text-sm"><span className="mb-1.5 block text-text-secondary">Alias ID</span><input required pattern="[a-z0-9][a-z0-9._-]{0,62}" disabled={!!aliases[form.slug]} value={form.slug} onChange={e => setForm({ ...form, slug: e.target.value })} className="w-full rounded-md border border-border bg-slate-900/60 px-3 py-2 font-mono text-sm focus:border-primary focus:outline-none" placeholder="fast-model" /></label><label className="block text-sm"><span className="mb-1.5 block text-text-secondary">Provider</span><input required value={form.provider} onChange={e => setForm({ ...form, provider: e.target.value })} className="w-full rounded-md border border-border bg-slate-900/60 px-3 py-2 font-mono text-sm focus:border-primary focus:outline-none" placeholder="openai" /></label><label className="block text-sm"><span className="mb-1.5 block text-text-secondary">Model</span><input required value={form.target} onChange={e => setForm({ ...form, target: e.target.value })} className="w-full rounded-md border border-border bg-slate-900/60 px-3 py-2 font-mono text-sm focus:border-primary focus:outline-none" placeholder="gpt-4o" /></label><label className="block text-sm"><span className="mb-1.5 block text-text-secondary">Additional aliases <span className="text-text-muted">(comma separated)</span></span><input value={form.additional} onChange={e => setForm({ ...form, additional: e.target.value })} className="w-full rounded-md border border-border bg-slate-900/60 px-3 py-2 text-sm focus:border-primary focus:outline-none" placeholder="default-model, chat-model" /></label><label className="block text-sm"><span className="mb-1.5 block text-text-secondary">Target selection</span><select value={form.selector} onChange={e => setForm({ ...form, selector: e.target.value })} className="w-full rounded-md border border-border bg-slate-900/60 px-3 py-2 text-sm focus:border-primary focus:outline-none"><option value="random">Random</option><option value="in_order">In order</option><option value="cost">Lowest cost</option><option value="latency">Lowest latency</option></select></label></div><div className="mt-6 flex justify-end gap-2"><Button type="button" variant="secondary" onClick={() => setForm(null)}>Cancel</Button><Button type="submit" isLoading={saving}>Save alias</Button></div></form></div>}
    </main></div>;
};
