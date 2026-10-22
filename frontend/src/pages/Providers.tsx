import React, { useCallback, useEffect, useRef, useState } from 'react';
import { CheckCircle2, Code2, Copy, ExternalLink, KeyRound, Link2Off, Loader2, Pencil, Plus, RefreshCw, Server, Trash2 } from 'lucide-react';
import { Link } from 'react-router-dom';
import { CatalogOrganizer } from '../components/CatalogOrganizer';
import { toast } from 'sonner';
import { Button } from '../components/ui/button';
import { Badge } from '../components/ui/badge';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '../components/ui/card';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '../components/ui/dialog';
import { Input } from '../components/ui/input';
import { Label } from '../components/ui/label';
import { Switch } from '../components/ui/switch';
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '../components/ui/table';
import { Skeleton } from '../components/ui/skeleton';
import { cancelOAuthSession, deleteOAuthCredentials, discoverProviderModels, getOAuthCredentialStatus, getOAuthProviders, getOAuthSession, getProviderPresets, getQuotaCheckerTypes, startOAuthSession, submitOAuthManualCode, submitOAuthPrompt, submitOAuthSelect, OAUTH_TERMINAL_STATUSES, deleteProvider, getProviders, getProvider, saveProvider, updateProvider, type OAuthProviderInfo, type OAuthSession, type ProviderConfig, type ProviderPreset, type QuotaCheckerType } from '../lib/api';

type ProviderModel = { type?: string; access_via?: string[]; [key: string]: unknown };
type FormState = { name: string; display_name: string; api_base_url: string; api_base_urls: Record<string, string>; models: Record<string, ProviderModel>; api_key: string; enabled: boolean; disable_cooldown: boolean; stall_cooldown: boolean; estimateTokens: boolean; useClaudeMasking: boolean; auto_compat: boolean; quota_type: string; quota_interval: string; protocol: string };
const blank: FormState = { name: '', display_name: '', api_base_url: '', api_base_urls: {}, models: {}, api_key: '', enabled: true, disable_cooldown: false, stall_cooldown: false, estimateTokens: false, useClaudeMasking: false, auto_compat: false, quota_type: '', quota_interval: '30', protocol: 'chat' };
const API_TYPES = ['chat', 'completions', 'messages', 'gemini', 'embeddings', 'transcriptions', 'speech', 'openai-images', 'responses'];
const TERMINAL: readonly string[] = OAUTH_TERMINAL_STATUSES;
/** Plexus `useProviderForm` status labels. */
const OAUTH_STATUS_LABELS: Record<string, string> = { in_progress: 'Starting', awaiting_auth: 'Awaiting browser', awaiting_prompt: 'Awaiting input', awaiting_manual_code: 'Awaiting code', awaiting_select: 'Awaiting method', success: 'Authenticated', error: 'Error', cancelled: 'Cancelled' };

export const Providers: React.FC = () => {
  const [providers, setProviders] = useState<Record<string, ProviderConfig>>({});
  const [form, setForm] = useState<FormState | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [editingSnapshot, setEditingSnapshot] = useState<ProviderConfig | null>(null);
  const [toggling, setToggling] = useState<Record<string, boolean>>({});
  const [error, setError] = useState<string | null>(null);
  const [oauthProviders, setOauthProviders] = useState<OAuthProviderInfo[]>([]);
  const [presets, setPresets] = useState<ProviderPreset[]>([]);
  const [quotaTypes, setQuotaTypes] = useState<QuotaCheckerType[]>([]);
  const [presetWarning, setPresetWarning] = useState('');
  const [discoveredModels, setDiscoveredModels] = useState<string[]>([]);
  const [discovering, setDiscovering] = useState(false);
  const discoverySeq = useRef(0);
  const discoveryUrl = useRef('');
  const discoveryAbort = useRef<AbortController | null>(null);
  const [oauthReady, setOauthReady] = useState<Record<string, boolean>>({});
  const [oauthBusy, setOauthBusy] = useState(false);
  const [session, setSession] = useState<OAuthSession | null>(null);
  const [promptValue, setPromptValue] = useState('');
  const [redirectValue, setRedirectValue] = useState('');
  const [selectValue, setSelectValue] = useState('');
  const [mode, setMode] = useState<'choose' | 'oauth' | 'apikey'>('choose');
  const [pickedProvider, setPickedProvider] = useState<OAuthProviderInfo | null>(null);
  const [accountId, setAccountId] = useState('');
  const pasteRef = useRef<HTMLInputElement>(null);

  const load = useCallback(async () => { setLoading(true); setError(null); try { setProviders(await getProviders()); } catch (e) { setError(e instanceof Error ? e.message : 'Unable to load providers'); } finally { setLoading(false); } }, []);
  useEffect(() => { void load(); void getOAuthProviders().then(setOauthProviders).catch(() => setOauthProviders([])); void getProviderPresets().then(setPresets).catch(() => setPresets([])); void getQuotaCheckerTypes().then(setQuotaTypes).catch(() => setQuotaTypes([])); }, [load]);
  useEffect(() => { oauthProviders.forEach(provider => { void getOAuthCredentialStatus(provider.id, 'default').then(status => setOauthReady(current => ({ ...current, [provider.id]: status.ready }))).catch(() => undefined); }); }, [oauthProviders]);

  // Poll the active session until it reaches a terminal state.
  useEffect(() => {
    if (!session || TERMINAL.includes(session.status)) return;
    const timer = window.setInterval(() => { void getOAuthSession(session.id).then(setSession).catch(() => undefined); }, 1000);
    return () => window.clearInterval(timer);
  }, [session]);

  // When authorization completes, register the provider automatically.
  useEffect(() => {
    if (session?.status !== 'success') return;
    void configure(session.providerId, session.accountId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session?.status]);

  useEffect(() => { if (session?.status === 'awaiting_manual_code') pasteRef.current?.focus(); }, [session?.status]);

  const resetFlow = () => {
    if (session && !TERMINAL.includes(session.status)) void cancelOAuthSession(session.id).catch(() => undefined).finally(() => setSession(s => s && s.status === 'cancelled' ? null : s));
    setSession(null); setPromptValue(''); setRedirectValue(''); setSelectValue(''); setPickedProvider(null); setAccountId(''); setMode('choose'); setOauthBusy(false); setError(null);
  };
  const closeDialog = (open: boolean) => { if (!open) { discoverySeq.current++; discoveryAbort.current?.abort(); discoveryAbort.current = null; discoveryUrl.current = ''; setDiscoveredModels([]); setDiscovering(false); setForm(null); resetFlow(); } };

  const startOAuth = async (provider: OAuthProviderInfo) => {
    setOauthBusy(true); setError(null);
    try {
      if (!/^[a-z0-9][a-z0-9._-]{0,62}$/.test(accountId.trim())) throw new Error('Use a lowercase account label of letters, numbers, dots, underscores or hyphens.');
      const s = await startOAuthSession(provider.id, accountId.trim());
      setSession(s);
    } catch (e) { setError(e instanceof Error ? e.message : 'Unable to start OAuth login'); } finally { setOauthBusy(false); }
  };
  // Plexus sends the pasted value verbatim; each provider validates it
  // (Anthropic `code#state`, Codex/ChatGPT redirect URLs, OpenRouter codes).
  const submitRedirect = async () => {
    if (!session || !redirectValue) return;
    setOauthBusy(true); setError(null);
    try { setSession(await submitOAuthManualCode(session.id, redirectValue)); setRedirectValue(''); }
    catch (e) { setError(e instanceof Error ? e.message : 'Failed to submit code'); } finally { setOauthBusy(false); }
  };
  const submitSelect = async () => {
    if (!session?.select) return;
    const value = selectValue || session.select.options[0]?.id;
    if (!value) return;
    setOauthBusy(true); setError(null);
    try { setSession(await submitOAuthSelect(session.id, value)); setSelectValue(''); }
    catch (e) { setError(e instanceof Error ? e.message : 'Failed to submit selection'); } finally { setOauthBusy(false); }
  };
  const submitPrompt = async () => {
    if (!session) return;
    setOauthBusy(true);
    try { setSession(await submitOAuthPrompt(session.id, promptValue)); setPromptValue(''); }
    catch (e) { setError(e instanceof Error ? e.message : 'Failed to submit prompt'); } finally { setOauthBusy(false); }
  };
  const configure = async (providerId: string, acct: string) => {
    const name = `${providerId}-${acct}`.toLowerCase().replace(/[^a-z0-9._-]+/g, '-').replace(/^-+|-+$/g, '');
    try {
      // A completed OAuth login must never replace an existing routing entry.
      const current = await getProviders();
      if (current[name]) {
        if (current[name].oauth_provider === providerId && current[name].oauth_account === acct) {
          setOauthReady(state => ({ ...state, [providerId]: true }));
          toast.success(`${providerId} account “${acct}” connected`);
          return;
        }
        throw new Error(`Provider “${name}” already exists. Choose another account label; existing routing was not changed.`);
      }
      await saveProvider(name, { display_name: `${providerId} OAuth (${acct})`, api_base_url: 'oauth://', oauth_provider: providerId, oauth_account: acct, api_key: 'oauth', enabled: true });
      await load();
      setOauthReady(current => ({ ...current, [providerId]: true }));
      toast.success(`${providerId} connected — provider “${name}” added`);
    } catch (e) { const m = e instanceof Error ? e.message : 'Unable to configure provider'; setError(m); toast.error(m); }
  };
  const disconnectOAuth = async (providerId: string) => {
    if (!window.confirm(`Disconnect ${providerId} account “default”?`)) return;
    setOauthBusy(true);
    try { await deleteOAuthCredentials(providerId, 'default'); setOauthReady(current => ({ ...current, [providerId]: false })); toast.success('OAuth account disconnected'); }
    catch (e) { toast.error(e instanceof Error ? e.message : 'Unable to disconnect OAuth account'); } finally { setOauthBusy(false); }
  };

  const edit = (name: string, provider?: ProviderConfig) => { discoverySeq.current++; discoveryAbort.current?.abort(); discoveryAbort.current = null; discoveryUrl.current = '';  setDiscovering(false); setMode('apikey'); setPresetWarning(''); setDiscoveredModels([]); setEditingSnapshot(null); if (provider) void getProvider(name).then(snapshot => setEditingSnapshot(snapshot)).catch(e => setError(e instanceof Error ? e.message : 'Unable to load provider revision')); const baseUrl = provider?.api_base_url; const urls = baseUrl && typeof baseUrl === 'object' ? { ...baseUrl } : typeof baseUrl === 'string' && baseUrl && !baseUrl.startsWith('oauth://') ? { [baseUrl.includes('anthropic.com') ? 'messages' : 'chat']: baseUrl } : {}; setForm({ ...blank, name, display_name: provider?.display_name ?? '', api_base_url: typeof baseUrl === 'string' && !baseUrl.startsWith('oauth://') ? baseUrl : '', api_base_urls: urls, models: Array.isArray(provider?.models) ? Object.fromEntries(provider.models.map(id => [id, { access_via: [] }])) : provider?.models && typeof provider.models === 'object' ? Object.fromEntries(Object.entries(provider.models).map(([id, model]) => [id, { ...model, access_via: Array.isArray(model.access_via) ? model.access_via : [] }])) : {}, api_key: '', enabled: provider?.enabled ?? true, disable_cooldown: provider?.disable_cooldown ?? false, stall_cooldown: provider?.stall_cooldown ?? false, estimateTokens: provider?.estimateTokens ?? false, useClaudeMasking: provider?.useClaudeMasking ?? false, auto_compat: provider?.auto_compat ?? false, quota_type: provider?.quota_checker?.type ?? '', quota_interval: String(provider?.quota_checker?.intervalMinutes ?? 30), protocol: 'chat' }); };
  const applyPreset = (id: string) => { const preset = presets.find(item => item.id === id); if (!preset || !form) return; const all = typeof preset.apiBaseUrl === 'string' ? { chat: preset.apiBaseUrl } : preset.apiBaseUrl; const endpoints = Object.fromEntries(Object.entries(all).filter(([type]) => API_TYPES.includes(type))); const protocol = endpoints.messages ? 'messages' : endpoints.chat ? 'chat' : Object.keys(endpoints)[0] ?? 'chat'; const url = endpoints[protocol]; setForm({ ...form, name: form.name || preset.suggestedProviderId || preset.id, display_name: form.display_name || preset.suggestedName || preset.name, api_base_url: url || '', api_base_urls: endpoints, protocol, auto_compat: !!preset.autoCompat, useClaudeMasking: preset.id === 'anthropic' }); setPresetWarning(Object.keys(all).length !== Object.keys(endpoints).length ? 'Unsupported preset endpoint types were omitted; review supported URLs before saving.' : ''); };
  const submit = async (event: React.FormEvent) => { event.preventDefault(); if (!form) return; const urls: Record<string, string> = {}; const seen = new Set<string>(); for (const [type, raw] of Object.entries(form.api_base_urls)) { const value = raw.trim(); if (!value) continue; let parsed: URL; try { parsed = new URL(value); if (!['http:', 'https:'].includes(parsed.protocol) || parsed.username || parsed.password) throw new Error(); } catch { setError(`Enter a valid HTTP or HTTPS URL for ${type}.`); return; } const normalized = parsed.toString().replace(/\/$/, ''); if (seen.has(type)) { setError(`Duplicate API type: ${type}.`); return; } seen.add(type); urls[type] = normalized; } if (Object.keys(urls).length === 0 && form.api_base_url.trim()) { try { const parsed = new URL(form.api_base_url.trim()); if (!['http:', 'https:'].includes(parsed.protocol) || parsed.username || parsed.password) throw new Error(); urls[form.protocol || 'chat'] = parsed.toString().replace(/\/$/, ''); } catch { setError('Enter a valid HTTP or HTTPS API base URL.'); return; } } if (Object.keys(urls).length === 0) { setError('Add at least one API base URL.'); return; } if (form.quota_type && !quotaTypes.some(item => item.type === form.quota_type) && form.quota_type !== providers[form.name]?.quota_checker?.type) { setError('Choose a supported quota checker type.'); return; } if (form.quota_type && (!Number.isInteger(Number(form.quota_interval)) || Number(form.quota_interval) < 1)) { setError('Quota checker interval must be a positive whole number of minutes.'); return; } const models: Record<string, ProviderModel> = {}; for (const [rawId, config] of Object.entries(form.models)) { const id = rawId.trim(); if (!id) continue; if (models[id]) { setError(`Duplicate model ID: ${id}.`); return; } const { type: _type, access_via: _accessVia, ...rest } = config; models[id] = { ...rest, ...(config.type ? { type: config.type } : {}), ...(config.access_via?.length ? { access_via: config.access_via } : {}) }; } setSaving(true); setError(null); try { const payload: Record<string, unknown> = { display_name: form.display_name || undefined, api_base_url: Object.keys(urls).length > 1 ? urls : urls[form.protocol] || Object.values(urls)[0], models, enabled: form.enabled, disable_cooldown: form.disable_cooldown, stall_cooldown: form.stall_cooldown, estimateTokens: form.estimateTokens, useClaudeMasking: form.useClaudeMasking, auto_compat: form.auto_compat }; if (form.api_key) payload.api_key = form.api_key; const currentProvider = providers[form.name] ? (await getProviders())[form.name] : undefined; if (editingSnapshot && currentProvider && JSON.stringify(currentProvider.quota_checker ?? null) !== JSON.stringify(editingSnapshot.quota_checker ?? null)) { throw new Error('Quota checker changed since this editor opened. Close and reopen to review the latest settings.'); } if (form.quota_type) { const existingQuota = currentProvider?.quota_checker; payload.quota_checker = { ...existingQuota, type: form.quota_type, id: existingQuota?.id || form.name, enabled: true, intervalMinutes: Number(form.quota_interval) }; } else if (providers[form.name]?.quota_checker) { const existingQuota = currentProvider?.quota_checker; if (existingQuota) payload.quota_checker = { ...existingQuota, enabled: false }; } if (providers[form.name]) { if (!editingSnapshot?.etag) throw new Error('Provider revision unavailable; reopen this editor'); await updateProvider(form.name, payload, editingSnapshot.etag); } else await saveProvider(form.name, payload); setForm(null); await load(); toast.success('Provider saved'); } catch (e) { const message = e instanceof Error ? e.message : 'Unable to save provider'; setError(message); toast.error(message); } finally { setSaving(false); } };
  const toggleEnabled = async (name: string, enabled: boolean) => {
    const current = providers[name];
    if (!current) return;
    setToggling(state => ({ ...state, [name]: true }));
    try {
      // PATCH merges fields server-side. Never round-trip secrets in the toggle request.
      await updateProvider(name, { enabled });
      setProviders(state => ({ ...state, [name]: { ...state[name], enabled } }));
    } catch (e) {
      toast.error(e instanceof Error ? e.message : 'Unable to update provider');
    } finally { setToggling(state => ({ ...state, [name]: false })); }
  };
  const remove = async (name: string) => { if (!window.confirm(`Delete provider “${name}”?`)) return; try { await deleteProvider(name); await load(); toast.success('Provider deleted'); } catch (e) { toast.error(e instanceof Error ? e.message : 'Unable to delete provider'); } };

  const sessionRunning = session && !TERMINAL.includes(session.status);
  const progressMessages = (session?.progress ?? []).slice(-3);
  const statusLabel = session ? OAUTH_STATUS_LABELS[session.status] ?? session.status : '';

  return <div className="w-full min-w-0 space-y-3">
    <header className="flex flex-wrap items-end justify-between gap-3 border-b pb-2"><div className="min-w-0"><p className="eyebrow">Configuration / Routing</p><h1 className="display">Providers</h1><p className="mt-1 text-xs text-muted-foreground">Upstream LLM providers routed by the gateway</p></div><div className="flex w-full flex-wrap gap-2 sm:w-auto"><Button variant="outline" onClick={() => { setPickedProvider(oauthProviders.find(provider => provider.id === 'anthropic') ?? oauthProviders[0] ?? null); setAccountId(''); setMode('oauth'); setForm(blank); }}><Plus /> Add OAuth account</Button><Button onClick={() => edit('', undefined)}><Plus /> Add provider</Button><Button variant="outline" onClick={() => void load()} disabled={loading} aria-label="Refresh providers"><RefreshCw className={loading ? 'animate-spin' : ''} /> Refresh</Button><Button variant="ghost" asChild><Link to="/providers/custom-checkers"><Code2 /> Custom checkers</Link></Button></div></header>
    {error && !form && <p role="alert" className=" border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">{error}</p>}

    {loading ? <Card><CardContent className="space-y-3 p-3">{[0, 1, 2].map(i => <Skeleton key={i} className="h-12 w-full" />)}</CardContent></Card> : Object.keys(providers).length === 0 ? <Card><CardContent className="py-8 text-center"><Server className="mx-auto text-muted-foreground" /><p className="mt-3 font-medium">No providers configured</p><p className="mt-1 text-xs text-muted-foreground">Add an upstream to start routing requests.</p></CardContent></Card> : <CatalogOrganizer kind="provider" ids={Object.keys(providers)} searchText={id => `${id} ${providers[id]?.display_name ?? ''}`}>{(visible, _organization, controls) => <Card className="min-w-0 overflow-hidden"><div className="overflow-x-auto"><Table className="min-w-[760px]"><TableHeader><TableRow className="bg-muted/40"><TableHead className="px-5">ID / Name</TableHead><TableHead>Status</TableHead><TableHead>Models</TableHead><TableHead>Quota / Balance</TableHead><TableHead>Organization</TableHead><TableHead className="text-right">Actions</TableHead></TableRow></TableHeader><TableBody>{visible.map(name => { const provider = providers[name]; return <TableRow key={name}><TableCell className="max-w-[440px] px-5"><button type="button" className="inline-flex max-w-full items-center gap-2 text-left" onClick={() => edit(name, provider)}><Pencil size={13} className="shrink-0 text-muted-foreground" /><span className="truncate font-semibold">{name}</span><span className="truncate text-muted-foreground">({provider.display_name || name})</span></button></TableCell><TableCell><Switch aria-label={`Enable ${name}`} checked={provider.enabled} disabled={!!toggling[name]} onCheckedChange={enabled => void toggleEnabled(name, enabled)} /></TableCell><TableCell>{provider.models && Object.keys(provider.models).length ? Object.keys(provider.models).length : <span className="text-muted-foreground" title="No explicit model inventory; legacy routes remain unrestricted">—</span>}</TableCell><TableCell className="text-muted-foreground">—</TableCell><TableCell>{controls(name)}</TableCell><TableCell><div className="flex justify-end gap-1"><Button variant="ghost" size="icon" aria-label={`Edit ${name}`} onClick={() => edit(name, provider)}><Pencil /></Button><Button variant="ghost" size="icon" className="text-destructive" aria-label={`Delete ${name}`} onClick={() => void remove(name)}><Trash2 /></Button></div></TableCell></TableRow>; })}</TableBody></Table></div></Card>}</CatalogOrganizer>}

    {oauthProviders.length > 0 && <Card><CardHeader><CardTitle>Connected OAuth accounts</CardTitle><CardDescription>Subscription sign-ins are stored server-side and never exposed as tokens. Add or re-authorize an account with “Add OAuth account”.</CardDescription></CardHeader><CardContent className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3 2xl:grid-cols-4">{oauthProviders.map(provider => { const ready = oauthReady[provider.id] === true; return <Card key={provider.id}><CardContent className="space-y-2 p-2"><div className="flex justify-between gap-2"><span className="font-medium">{provider.name}</span><Badge variant={ready ? 'success' : 'secondary'}>{ready ? 'Connected' : 'Not connected'}</Badge></div><p className="font-mono text-xs text-muted-foreground">{provider.id}</p>{ready ? <Button variant="destructive" size="sm" onClick={() => void disconnectOAuth(provider.id)} disabled={oauthBusy}><Link2Off /> Disconnect</Button> : <Button size="sm" variant="outline" onClick={() => { setPickedProvider(provider); setMode('oauth'); setForm(blank); }}><Plus /> Connect</Button>}</CardContent></Card>; })}</CardContent></Card>}

    <Dialog open={!!form} onOpenChange={closeDialog}>
      <DialogContent className="max-h-[90vh] max-w-3xl overflow-y-auto">
        {mode === 'choose' && form && <>
          <DialogHeader><DialogTitle>Add provider</DialogTitle><DialogDescription>Sign in with a subscription account, or connect an API-key upstream.</DialogDescription></DialogHeader>
          <div className="space-y-1.5">
            {oauthProviders.map(provider => (
              <button key={provider.id} type="button" onClick={() => { setPickedProvider(provider); setMode('oauth'); }} disabled={oauthBusy} className="flex w-full min-w-0 items-center gap-3 border px-3 py-2.5 text-left transition-colors hover:border-primary/50 hover:bg-accent focus-visible:outline-2 focus-visible:outline-primary">
                <ExternalLink className="shrink-0 text-muted-foreground" size={16} />
                <span className="min-w-0 flex-1 truncate text-xs font-medium">{provider.name}</span>
                {oauthReady[provider.id] ? <Badge className="shrink-0" variant="success">Connected</Badge> : <span className="shrink-0 text-xs text-muted-foreground">OAuth</span>}
              </button>
            ))}
            <button type="button" onClick={() => setMode('apikey')} disabled={oauthBusy} className="flex w-full items-center gap-3 border px-3 py-3 text-left transition-colors hover:border-primary/50 hover:bg-accent focus-visible:outline-2 focus-visible:outline-primary">
              <KeyRound className="text-primary" size={18} />
              <span className="min-w-0 flex-1"><span className="block font-medium">API key provider</span><span className="block text-xs text-muted-foreground">Custom upstream endpoint</span></span>
            </button>
          </div>
        </>}

        {mode === 'oauth' && form && <>
          <DialogHeader><p className="eyebrow">Add OAuth account</p><DialogTitle>Add OAuth account</DialogTitle><DialogDescription>{session ? 'Finish authorization in your browser, then return here.' : 'Create a separate OAuth credential for Trefoil routing. Tokens stay on the server.'}</DialogDescription></DialogHeader>
          <div className="space-y-4">
            {error && <p role="alert" className="border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">{error}</p>}
            {!session && <><div className="space-y-2"><Label htmlFor="oauth-provider-select">Provider</Label><select id="oauth-provider-select" className="h-7 w-full  border border-input bg-background px-3 text-xs" value={pickedProvider?.id ?? ''} onChange={e => setPickedProvider(oauthProviders.find(provider => provider.id === e.target.value) ?? null)}><option value="">Select provider</option>{oauthProviders.map(provider => <option key={provider.id} value={provider.id}>{provider.name}</option>)}</select></div><div className="space-y-2"><Label htmlFor="oauth-acct">Account label</Label><Input id="oauth-acct" value={accountId} onChange={e => setAccountId(e.target.value)} placeholder="e.g. personal, work, team-2" /><p className="text-xs text-muted-foreground">A separate credential per label; no password is stored in this form.</p></div></>}
            {!session && <Button className="h-auto min-h-9 w-full whitespace-normal px-3 py-2 text-center" onClick={() => pickedProvider && void startOAuth(pickedProvider)} disabled={oauthBusy || !pickedProvider || !accountId.trim()}>{oauthBusy ? <Loader2 className="animate-spin" /> : <ExternalLink />}Continue to login</Button>}
            {session?.status === 'awaiting_select' && session.select && <div className="space-y-2">
              <Label htmlFor="oauth-select">{session.select.message}</Label>
              <div className="flex min-w-0 flex-col gap-2 sm:flex-row">
                <select id="oauth-select" className="h-7 min-w-0 flex-1  border border-input bg-background px-3 text-xs" value={selectValue || session.select.options[0]?.id || ''} onChange={e => setSelectValue(e.target.value)}>{session.select.options.map(option => <option key={option.id} value={option.id} title={option.description}>{option.label}</option>)}</select>
                <Button className="shrink-0" onClick={() => void submitSelect()} disabled={oauthBusy}>Continue</Button>
              </div>
            </div>}
            {session?.authInfo && <div className="space-y-2 border-b pb-4 text-xs"><p className="font-medium">Authorize in your browser</p>
              <div className="flex flex-wrap gap-2"><Button size="sm" asChild><a href={session.authInfo.url} target="_blank" rel="noreferrer"><ExternalLink /> Open authorization page</a></Button><Button size="sm" variant="outline" onClick={() => { void navigator.clipboard.writeText(session.authInfo!.url); toast.success('Authorization URL copied'); }}><Copy /> Copy URL</Button></div>
              <Input readOnly aria-label="Authorization URL" className="font-mono text-xs" value={session.authInfo.url} />
              {session.authInfo.instructions && <p className="text-xs text-muted-foreground">{session.authInfo.instructions}</p>}
            </div>}
            {session?.prompt && <div className="space-y-2"><Label htmlFor="oauth-prompt">{session.prompt.message}</Label><div className="flex min-w-0 flex-col gap-2 sm:flex-row"><Input id="oauth-prompt" className="min-w-0 flex-1" placeholder={session.prompt.placeholder} value={promptValue} onChange={e => setPromptValue(e.target.value)} onKeyDown={e => { if (e.key === 'Enter') { e.preventDefault(); void submitPrompt(); } }} /><Button className="shrink-0" onClick={() => void submitPrompt()} disabled={oauthBusy || (!session.prompt.allowEmpty && !promptValue)}>Submit</Button></div></div>}
            {session?.status === 'awaiting_manual_code' && <div className="space-y-2">
              <Label htmlFor="oauth-manual-code">{session.manualCode?.message ?? 'Paste redirect URL or code'}</Label>
              <div className="flex min-w-0 flex-col gap-2 sm:flex-row">
                <Input id="oauth-manual-code" ref={pasteRef} className="min-w-0 flex-1 font-mono text-xs" placeholder={session.manualCode?.placeholder ?? 'https://...'} value={redirectValue} onChange={e => { setRedirectValue(e.target.value); setError(null); }} onKeyDown={e => { if (e.key === 'Enter') { e.preventDefault(); void submitRedirect(); } }} />
                <Button className="shrink-0" onClick={() => void submitRedirect()} disabled={oauthBusy || !redirectValue}>{oauthBusy ? <Loader2 className="animate-spin" /> : 'Submit'}</Button>
              </div>
              <p className="text-xs text-muted-foreground">If the browser lands on a localhost page that does not load, copy the full address-bar URL. Never paste a password.</p>
            </div>}
            {progressMessages.length > 0 && <div className="space-y-1 border-l-2 border-border pl-3 text-xs text-muted-foreground"><p className="font-medium">Progress</p>{progressMessages.map((m, i) => <p key={`${m}-${i}`}>{m}</p>)}</div>}
            {session && <div role="status" className="flex items-center gap-2 border-t pt-3 text-xs">{sessionRunning && <><Loader2 className="animate-spin text-primary" size={15} />{statusLabel}…</>}{session.status === 'success' && <><CheckCircle2 className="text-success" size={16} />Authentication complete. Tokens stored securely on the server.</>}{session.status === 'cancelled' && <span className="text-muted-foreground">Sign-in cancelled.</span>}{session.status === 'error' && <span className="text-destructive">Sign-in failed. Start again to retry.</span>}</div>}
            {session?.error && session.status !== 'cancelled' && <p role="alert" className="text-xs text-destructive">{session.error}</p>}
          </div>
          <DialogFooter className="flex-col gap-2 sm:flex-row sm:space-x-0">{session ? <>{sessionRunning && <Button type="button" variant="ghost" onClick={() => void cancelOAuthSession(session.id).then(setSession).catch(e => setError(e instanceof Error ? e.message : 'Failed to cancel session'))} disabled={oauthBusy}>Cancel</Button>}<Button type="button" variant="outline" onClick={resetFlow}>{session.status === 'success' ? 'Add another' : 'Start over'}</Button></> : <Button type="button" variant="ghost" onClick={() => setMode('choose')}>Back</Button>}<Button type="button" variant="outline" onClick={() => closeDialog(false)}>Close</Button></DialogFooter>
        </>}

        {mode === 'apikey' && form && <form onSubmit={submit} className="space-y-3">
          <DialogHeader><DialogTitle>{form.name ? 'Edit provider' : 'Add API-key provider'}</DialogTitle><DialogDescription>Configure the upstream endpoint and credentials.</DialogDescription></DialogHeader>
          {error && <p role="alert" className="border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">{error}</p>}
          <div className="space-y-4">
            <div className="space-y-2"><Label htmlFor="provider-id">Provider ID</Label><Input id="provider-id" required pattern="[a-z0-9][a-z0-9._-]{0,62}" disabled={!!providers[form.name]} value={form.name} onChange={e => setForm({ ...form, name: e.target.value })} placeholder="openai" /></div>
            {!providers[form.name] && <div className="space-y-2"><Label htmlFor="provider-preset">Start from provider preset</Label><select id="provider-preset" className="h-7 w-full  border border-input bg-background px-3 text-xs" defaultValue="" onChange={e => applyPreset(e.target.value)}><option value="">Custom (blank)</option>{presets.map(preset => <option key={preset.id} value={preset.id}>{preset.name}</option>)}</select><p className="text-xs text-muted-foreground">Presets prefill a base URL and supported compatibility settings only.</p>{presetWarning && <p role="status" className=" border border-amber-500/40 bg-amber-500/10 p-2 text-xs">{presetWarning}</p>}</div>}
            <div className="space-y-2"><Label htmlFor="provider-display">Display name</Label><Input id="provider-display" value={form.display_name} onChange={e => setForm({ ...form, display_name: e.target.value })} placeholder="OpenAI" /></div>
            <div className="space-y-2"><Label>API base URLs</Label><p className="text-xs text-muted-foreground">Set a protocol-specific HTTP(S) base URL for each API. Blank rows are ignored.</p>{Object.entries(form.api_base_urls).map(([type, url]) => <div key={type} className="grid gap-2 sm:grid-cols-[minmax(0,0.7fr)_minmax(0,1.3fr)_auto]"><select aria-label="API type" className="h-7  border border-input bg-background px-3 text-xs" value={type} onChange={e => { const next = e.target.value; if (form.api_base_urls[next] !== undefined && next !== type) { setError(`API type ${next} already exists.`); return; } const entries = { ...form.api_base_urls }; delete entries[type]; entries[next] = url; setForm({ ...form, api_base_urls: entries, protocol: form.protocol === type ? next : form.protocol }); setError(null); }}><option value={type}>{type}</option>{API_TYPES.filter(t => t === type || form.api_base_urls[t] === undefined).map(t => <option key={t} value={t}>{t}</option>)}</select><Input aria-label={`${type} base URL`} value={url} onChange={e => { if (type === 'chat') { discoverySeq.current++; discoveryAbort.current?.abort(); discoveryAbort.current = null; discoveryUrl.current = '';  setDiscoveredModels([]); setDiscovering(false); } setForm({ ...form, api_base_urls: { ...form.api_base_urls, [type]: e.target.value }, ...(form.protocol === type ? { api_base_url: e.target.value } : {}) }); }} placeholder="https://api.example.com/v1" /><Button type="button" variant="ghost" size="icon" aria-label={`Remove ${type} URL`} onClick={() => { const urls = { ...form.api_base_urls }; delete urls[type]; setForm({ ...form, api_base_urls: urls }); }}><Trash2 /></Button></div>)}<Button type="button" variant="outline" size="sm" disabled={API_TYPES.every(t => form.api_base_urls[t] !== undefined)} onClick={() => { const type = API_TYPES.find(t => form.api_base_urls[t] === undefined); if (type) setForm({ ...form, api_base_urls: { ...form.api_base_urls, [type]: '' }, protocol: type }); }}><Plus /> Add API URL</Button></div>
            <div className="space-y-2"><Label>Models</Label><p className="text-xs text-muted-foreground">Explicit model IDs affect routing. Access via is used by aliases with API-match priority to prefer matching targets; if none match, existing cross-format fallback remains. Direct provider/model routes are unchanged.</p>{Object.entries(form.models).map(([id, model]) => <div key={id} className="space-y-2  border p-3"><div className="grid gap-2 sm:grid-cols-[1fr_auto]"><Input aria-label="Model ID" value={id} onChange={e => { const models = { ...form.models }; delete models[id]; models[e.target.value] = model; setForm({ ...form, models }); }} placeholder="model-id" /><Button type="button" variant="ghost" size="icon" aria-label={`Remove model ${id || 'row'}`} onClick={() => { const models = { ...form.models }; delete models[id]; setForm({ ...form, models }); }}><Trash2 /></Button></div><div className="grid gap-2 sm:grid-cols-2"><select aria-label={`Type for ${id || 'model'}`} className="h-7  border border-input bg-background px-3 text-xs" value={model.type ?? 'text'} onChange={e => setForm({ ...form, models: { ...form.models, [id]: { ...model, type: e.target.value } } })}><option value="text">Text</option><option value="embeddings">Embeddings</option><option value="transcriptions">Transcriptions</option><option value="speech">Speech</option><option value="image">Image</option></select><Input aria-label={`Access via for ${id || 'model'}`} value={(model.access_via ?? []).join(', ')} onChange={e => setForm({ ...form, models: { ...form.models, [id]: { ...model, access_via: e.target.value.split(',').map(value => value.trim()).filter(Boolean) } } })} placeholder="Access via (comma-separated)" /></div></div>)}<div className="flex flex-wrap gap-2"><Button type="button" variant="outline" size="sm" disabled={discovering || form.api_base_urls.chat !== "https://openrouter.ai/api/v1"} onClick={() => { setDiscovering(true); setError(null); setDiscoveredModels([]); let url: URL; try { url = new URL(form.api_base_urls.chat); url.pathname = `${url.pathname.replace(/\/$/, '')}/models`; } catch { setError('Enter a valid chat API URL before discovery.'); setDiscovering(false); return; } const sequence = ++discoverySeq.current; discoveryAbort.current?.abort(); const controller = new AbortController(); discoveryAbort.current = controller; discoveryUrl.current = form.api_base_urls.chat; void discoverProviderModels(url.toString(), undefined, controller.signal).then(models => { if (sequence === discoverySeq.current && discoveryUrl.current === form.api_base_urls.chat) setDiscoveredModels(models); }).catch(cause => { if (sequence === discoverySeq.current && !(cause instanceof DOMException && cause.name === 'AbortError')) setError(cause instanceof Error ? cause.message : 'Unable to discover models'); }).finally(() => { if (sequence === discoverySeq.current) { discoveryAbort.current = null; setDiscovering(false); } }); }}>{discovering ? 'Fetching…' : 'Fetch Models (preview)'}</Button>{discoveredModels.length > 0 && <span role="status" className="text-xs text-muted-foreground">{discoveredModels.length} discovered; no changes saved yet</span>}</div>{discoveredModels.length > 0 && <div className="max-h-40 space-y-1 overflow-y-auto  border p-2 text-xs">{discoveredModels.map(id => <div key={id} className="flex items-center justify-between gap-2"><span className="min-w-0 truncate font-mono" title={id}>{id}</span><Button type="button" size="sm" variant="ghost" disabled={form.models[id] !== undefined} onClick={() => setForm({ ...form, models: { ...form.models, [id]: { type: 'text' } } })}>{form.models[id] ? 'Added' : 'Add'}</Button></div>)}</div>}<Button type="button" variant="outline" size="sm" onClick={() => { let key = ''; let index = Object.keys(form.models).length + 1; do { key = `model-${index++}`; } while (form.models[key] !== undefined); setForm({ ...form, models: { ...form.models, [key]: { type: 'text' } } }); }}><Plus /> Add model</Button></div>
            <div className="space-y-2"><Label htmlFor="provider-key">API key {form.name && providers[form.name] ? '(leave blank to keep current)' : ''}</Label><Input id="provider-key" required={!form.name || !providers[form.name]} type="password" value={form.api_key} onChange={e => setForm({ ...form, api_key: e.target.value })} /></div>
            <div className="flex items-center gap-3"><Switch id="provider-enabled" checked={form.enabled} onCheckedChange={enabled => setForm({ ...form, enabled })} /><Label htmlFor="provider-enabled">Enabled</Label></div>
            <details className=" border p-3"><summary className="cursor-pointer text-xs font-medium">Advanced provider settings</summary><div className="mt-3 space-y-3">
              <div className="flex items-center gap-3"><Switch id="provider-disable-cooldown" checked={form.disable_cooldown} onCheckedChange={value => setForm({ ...form, disable_cooldown: value })} /><Label htmlFor="provider-disable-cooldown">Disable failure cooldown</Label></div>
              <div className="flex items-center gap-3"><Switch id="provider-stall-cooldown" checked={form.stall_cooldown} onCheckedChange={value => setForm({ ...form, stall_cooldown: value })} /><Label htmlFor="provider-stall-cooldown">Cooldown on stalled responses</Label></div>
              <div className="flex items-center gap-3"><Switch id="provider-estimate-tokens" checked={form.estimateTokens} onCheckedChange={value => setForm({ ...form, estimateTokens: value })} /><Label htmlFor="provider-estimate-tokens">Estimate token usage</Label></div>
              <div className="flex items-center gap-3"><Switch id="provider-claude-masking" checked={form.useClaudeMasking} onCheckedChange={value => setForm({ ...form, useClaudeMasking: value })} /><Label htmlFor="provider-claude-masking">Claude compatibility masking</Label></div>
              <div className="flex items-center gap-3"><Switch id="provider-auto-compat" checked={form.auto_compat} onCheckedChange={value => setForm({ ...form, auto_compat: value })} /><Label htmlFor="provider-auto-compat">Automatic compatibility</Label></div>
              <div className="grid gap-2 sm:grid-cols-2"><div className="space-y-1"><Label htmlFor="provider-quota-type">Quota checker type</Label><select id="provider-quota-type" className="h-7 w-full  border border-input bg-background px-3 text-xs" value={form.quota_type} onChange={e => setForm({ ...form, quota_type: e.target.value })}><option value="">None (disable)</option>{form.quota_type && !quotaTypes.some(item => item.type === form.quota_type) && <option value={form.quota_type}>Existing: {form.quota_type}</option>}{quotaTypes.map(item => <option key={item.type} value={item.type}>{item.displayName}</option>)}</select></div><div className="space-y-1"><Label htmlFor="provider-quota-interval">Check interval (minutes)</Label><Input id="provider-quota-interval" type="number" min="1" step="1" disabled={!form.quota_type} value={form.quota_interval} onChange={e => setForm({ ...form, quota_interval: e.target.value })} /></div></div>
            </div></details>
          </div>
          <DialogFooter><Button type="button" variant="ghost" onClick={() => setMode(form.name ? 'choose' : 'choose')}>Back</Button><Button type="submit" disabled={saving}>{saving ? 'Saving…' : 'Save provider'}</Button></DialogFooter>
        </form>}
      </DialogContent>
    </Dialog>
  </div>;
};
