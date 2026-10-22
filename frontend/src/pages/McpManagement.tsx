import React, { useCallback, useEffect, useState } from 'react';
import { KeyRound, Pencil, Plus, RefreshCw, Server, Trash2 } from 'lucide-react';
import { toast } from 'sonner';
import {
  addMcpKey, deleteMcpKey, deleteMcpServer, getMcpKeys, getMcpServers,
  saveMcpServer, type McpKey, type McpServerConfig,
} from '../lib/api';
import { cn } from '../lib/utils';
import { Button } from '../components/ui/button';
import { Card, CardContent } from '../components/ui/card';
import { Badge } from '../components/ui/badge';
import { Input } from '../components/ui/input';
import { Label } from '../components/ui/label';
import { Switch } from '../components/ui/switch';
import { Skeleton } from '../components/ui/skeleton';
import {
  Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle,
} from '../components/ui/dialog';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '../components/ui/select';

type Form = { name: string; upstreamUrl: string; mode: string; enabled: boolean; authScheme: string; launcher: string; packageName: string; args: string; env: string };
const validName = /^[a-z0-9][a-z0-9._-]{0,62}$/;

const FIELDS = [
  ['name', 'Server name', 'mcp-tools'],
  ['upstreamUrl', 'Upstream URL', 'https://tools.example.com/mcp'],
  ['authScheme', 'Auth scheme', 'bearer'],
  ['launcher', 'Launcher', 'npx'],
  ['packageName', 'Package name', '@example/mcp-server'],
  ['args', 'Arguments', '--readonly'],
  ['env', 'Environment', 'KEY=value'],
] as const;

export const McpManagement: React.FC = () => {
  const [servers, setServers] = useState<Record<string, McpServerConfig>>({});
  const [keys, setKeys] = useState<Record<string, McpKey[]>>({});
  const [form, setForm] = useState<Form | null>(null);
  const [newKey, setNewKey] = useState<Record<string, string>>({});
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const data = await getMcpServers();
      setServers(data);
      const loaded = await Promise.all(Object.keys(data).map(async (name) => [name, await getMcpKeys(name)] as const));
      setKeys(Object.fromEntries(loaded));
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Unable to load MCP servers');
    } finally {
      setLoading(false);
    }
  }, []);
  useEffect(() => { void load(); }, [load]);

  const edit = (name: string, server?: McpServerConfig) =>
    setForm({ name, upstreamUrl: server?.upstreamUrl ?? '', mode: server?.mode ?? 'remote_http', enabled: server?.enabled ?? true, authScheme: server?.authScheme ?? '', launcher: server?.launcher ?? '', packageName: server?.packageName ?? '', args: server?.args ?? '', env: server?.env ?? '' });

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!form) return;
    const name = form.name.trim();
    if (!validName.test(name)) {
      setError('Server name must use lowercase letters, numbers, dots, underscores, or hyphens.');
      return;
    }
    setSaving(true);
    setError(null);
    try {
      await saveMcpServer(name, { upstream_url: form.upstreamUrl, mode: form.mode, enabled: form.enabled, auth_scheme: form.authScheme || undefined, launcher: form.launcher || undefined, package_name: form.packageName || undefined, args: form.args || undefined, env: form.env || undefined }, !!servers[name]);
      setForm(null);
      toast.success(`MCP server “${name}” saved`);
      await load();
    } catch (e) {
      const message = e instanceof Error ? e.message : 'Unable to save MCP server';
      setError(message);
      toast.error(message);
    } finally {
      setSaving(false);
    }
  };

  const remove = async (name: string) => {
    if (!window.confirm(`Delete MCP server “${name}”?`)) return;
    try {
      await deleteMcpServer(name);
      toast.success(`MCP server “${name}” deleted`);
      await load();
    } catch (e) {
      toast.error(e instanceof Error ? e.message : 'Unable to delete MCP server');
    }
  };

  const addKey = async (name: string) => {
    const key = newKey[name]?.trim();
    if (!key) return;
    try {
      await addMcpKey(name, key);
      setNewKey({ ...newKey, [name]: '' });
      toast.success('MCP key added');
      await load();
    } catch (e) {
      toast.error(e instanceof Error ? e.message : 'Unable to add MCP key');
    }
  };

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="font-heading text-base font-semibold">MCP servers</h1>
          <p className="text-xs text-muted-foreground">Manage remote and local tool integrations</p>
        </div>
        <div className="flex gap-2">
          <Button variant="outline" size="sm" onClick={() => void load()} disabled={loading}>
            <RefreshCw className={loading ? 'animate-spin' : ''} /> Refresh
          </Button>
          <Button size="sm" onClick={() => edit('')}>
            <Plus /> Add server
          </Button>
        </div>
      </div>

      {error && (
        <div role="alert" className=" border border-destructive/40 bg-destructive/10 p-3 text-xs text-red-300">
          {error}
        </div>
      )}

      {loading ? (
        <div className="grid gap-3 lg:grid-cols-4">
          <Skeleton className="h-52" />
          <Skeleton className="h-52" />
        </div>
      ) : Object.keys(servers).length === 0 ? (
        <Card>
          <CardContent className="flex flex-col items-center gap-2 py-8 text-center">
            <Server className="h-7 w-7 text-muted-foreground" />
            <p className="font-medium">No MCP servers configured</p>
            <p className="text-xs text-muted-foreground">Add a server to expose tools to the gateway.</p>
          </CardContent>
        </Card>
      ) : (
        <div className="grid gap-3 lg:grid-cols-2 2xl:grid-cols-3">
          {Object.entries(servers).map(([name, server]) => (
            <Card key={name}>
              <CardContent className="p-3">
                <div className="flex items-start justify-between gap-3">
                  <div className="flex min-w-0 items-center gap-3">
                    <div className=" bg-primary/10 p-1.5 text-primary"><Server className="h-4 w-4" /></div>
                    <div className="min-w-0">
                      <h2 className="truncate text-xs font-semibold">{name}</h2>
                      <p className="truncate font-mono text-xs text-muted-foreground">{server.mode} · {server.upstreamUrl || 'local process'}</p>
                    </div>
                  </div>
                  <Badge variant={server.enabled ? 'success' : 'secondary'}>{server.enabled ? 'Enabled' : 'Disabled'}</Badge>
                </div>

                <div className="mt-5 border-t pt-4">
                  <p className="flex items-center gap-2 text-xs text-muted-foreground">
                    <KeyRound className="h-3.5 w-3.5" /> {keys[name]?.length ?? 0} access keys
                  </p>
                  <div className="mt-3 flex gap-2">
                    <Input
                      value={newKey[name] ?? ''}
                      onChange={(e) => setNewKey({ ...newKey, [name]: e.target.value })}
                      placeholder="Add access key"
                      className="font-mono text-xs"
                    />
                    <Button size="sm" onClick={() => void addKey(name)}>Add</Button>
                  </div>
                  {keys[name]?.map((key) => (
                    <div key={key.id} className="mt-2 flex items-center justify-between  bg-muted/60 px-2 py-1.5 font-mono text-xs text-muted-foreground">
                      <span className="truncate">{key.key}</span>
                      <button
                        aria-label={`Delete MCP key ${key.id}`}
                        onClick={() => void deleteMcpKey(name, key.id).then(load).catch((e) => toast.error(e instanceof Error ? e.message : 'Unable to delete MCP key'))}
                        className="cursor-pointer text-red-400 hover:text-red-300"
                      >
                        <Trash2 className="h-3.5 w-3.5" />
                      </button>
                    </div>
                  ))}
                </div>

                <div className="mt-5 flex justify-end gap-2 border-t pt-4">
                  <Button variant="ghost" size="sm" onClick={() => edit(name, server)}><Pencil /> Edit</Button>
                  <Button variant="destructive" size="sm" onClick={() => void remove(name)}><Trash2 /> Delete</Button>
                </div>
              </CardContent>
            </Card>
          ))}
        </div>
      )}

      {form && (
        <Dialog open onOpenChange={(open) => !open && setForm(null)}>
          <DialogContent>
            <form onSubmit={submit} className="space-y-4">
              <DialogHeader>
                <DialogTitle>{form.name ? 'Edit MCP server' : 'Add MCP server'}</DialogTitle>
              </DialogHeader>
              <div className="space-y-4">
                {FIELDS.map(([field, label, placeholder]) => (
                  <div key={field} className="space-y-1.5">
                    <Label htmlFor={`mcp-${field}`}>{label}</Label>
                    <Input
                      id={`mcp-${field}`}
                      required={field === 'name' || field === 'upstreamUrl'}
                      disabled={field === 'name' && !!servers[form.name]}
                      value={form[field]}
                      onChange={(e) => setForm({ ...form, [field]: e.target.value })}
                      placeholder={placeholder}
                      className="font-mono"
                    />
                  </div>
                ))}
                <div className="space-y-1.5">
                  <Label>Mode</Label>
                  <Select value={form.mode} onValueChange={(value) => setForm({ ...form, mode: value })}>
                    <SelectTrigger><SelectValue /></SelectTrigger>
                    <SelectContent>
                      <SelectItem value="remote_http">Remote HTTP</SelectItem>
                      <SelectItem value="local_process">Local process</SelectItem>
                    </SelectContent>
                  </Select>
                </div>
                <label className={cn('flex items-center gap-2 text-xs')}>
                  <Switch checked={form.enabled} onCheckedChange={(checked) => setForm({ ...form, enabled: checked })} />
                  Enabled
                </label>
              </div>
              <DialogFooter>
                <Button type="button" variant="outline" onClick={() => setForm(null)}>Cancel</Button>
                <Button type="submit" disabled={saving}>{saving ? 'Saving…' : 'Save server'}</Button>
              </DialogFooter>
            </form>
          </DialogContent>
        </Dialog>
      )}
    </div>
  );
};
