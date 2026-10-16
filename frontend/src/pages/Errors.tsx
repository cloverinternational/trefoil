import React, { useCallback, useEffect, useState } from 'react';
import { AlertTriangle, ChevronLeft, ChevronRight, Clock, RefreshCw, Trash2 } from 'lucide-react';
import { useAuth } from '../contexts/AuthContext';
import { deleteError, deleteErrors, getErrors, type InferenceError } from '../lib/api';
import { Button } from '../components/ui/button';

const formatTime = (value: number | string | null | undefined) => {
  if (value == null || value === '') return '—';
  const numeric = typeof value === 'number' ? value : Number(value);
  const date = Number.isFinite(numeric)
    ? new Date(numeric < 1e10 ? numeric * 1000 : numeric)
    : new Date(value);
  return Number.isNaN(date.getTime()) ? '—' : date.toLocaleString();
};

const formatDetails = (value: string | null) => {
  if (!value) return 'No additional details available.';
  try {
    return JSON.stringify(JSON.parse(value), null, 2);
  } catch {
    return value;
  }
};

export const Errors: React.FC = () => {
  const { isAdmin, principal } = useAuth();
  const [errors, setErrors] = useState<InferenceError[]>([]);
  const [offset, setOffset] = useState(0);
  const [total, setTotal] = useState(0);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [mutating, setMutating] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const fetchErrors = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const result = await getErrors(50, offset);
      setErrors(result.data);
      setTotal(result.total);
      setSelectedId((current) => current && result.data.some((item) => item.requestId === current)
        ? current
        : result.data[0]?.requestId ?? null);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : 'Unable to load inference errors');
    } finally {
      setLoading(false);
    }
  }, [offset]);

  useEffect(() => { void fetchErrors(); }, [fetchErrors]);

  const selected = errors.find((item) => item.requestId === selectedId) ?? null;

  const removeOne = async (event: React.MouseEvent, requestId: string) => {
    event.stopPropagation();
    if (!isAdmin || !window.confirm('Delete this inference error?')) return;
    setMutating(true);
    setError(null);
    try {
      await deleteError(requestId);
      setErrors((current) => current.filter((item) => item.requestId !== requestId));
      setTotal((current) => Math.max(0, current - 1));
      if (selectedId === requestId) setSelectedId(null);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : 'Unable to delete inference error');
    } finally {
      setMutating(false);
    }
  };

  const removeAll = async () => {
    if (!isAdmin || !total || !window.confirm('Delete all inference errors?')) return;
    setMutating(true);
    setError(null);
    try {
      await deleteErrors();
      setErrors([]);
      setTotal(0);
      setOffset(0);
      setSelectedId(null);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : 'Unable to clear inference errors');
    } finally {
      setMutating(false);
    }
  };

  return (
    <main className="flex min-h-[calc(100vh-3rem)] flex-col">
      <header className="flex shrink-0 items-center justify-between gap-4 border-b border-border px-4 py-4 sm:px-6">
        <div>
          <h1 className="flex items-center gap-2 text-2xl font-semibold"><AlertTriangle className="size-5 text-destructive" />Errors</h1>
          <p className="mt-1 text-sm text-muted-foreground">{principal?.role === 'limited' && principal.keyName ? `Errors for key “${principal.keyName}” only` : 'Recent inference errors'}</p>
        </div>
        <div className="flex shrink-0 items-center gap-2">
          {isAdmin && <Button variant="destructive" size="sm" onClick={() => void removeAll()} disabled={!total || loading || mutating}><Trash2 />Delete All</Button>}
          <Button variant="outline" size="sm" onClick={() => void fetchErrors()} disabled={loading || mutating}><RefreshCw className={loading ? 'animate-spin' : ''} />Refresh</Button>
        </div>
      </header>

      {error && <div role="alert" className="mx-4 mt-3 rounded-md border border-destructive/30 bg-destructive/10 p-3 text-sm text-destructive sm:mx-6">{error}<Button variant="link" onClick={() => void fetchErrors()}>Retry</Button></div>}

      <div className="flex min-h-0 flex-1 flex-col overflow-hidden border-b border-border md:flex-row">
        <section aria-label="Recent errors" className="flex max-h-[34vh] w-full shrink-0 flex-col border-b border-border bg-card md:max-h-none md:w-80 md:border-b-0 md:border-r">
          <div className="border-b border-border px-4 py-4 text-xs font-bold uppercase tracking-wider text-muted-foreground">Recent Errors</div>
          <div className="flex-1 overflow-y-auto p-2">
            {loading && <div className="space-y-2 p-2" aria-label="Loading errors">{[1, 2, 3].map((item) => <div key={item} className="h-16 animate-pulse rounded-md bg-muted" />)}</div>}
            {!loading && errors.map((item) => <div key={item.id} role="button" tabIndex={0} aria-pressed={selectedId === item.requestId} onClick={() => setSelectedId(item.requestId)} onKeyDown={(event) => { if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); setSelectedId(item.requestId); } }} className={`group mb-2 cursor-pointer rounded-md border p-3 transition-colors hover:bg-accent ${selectedId === item.requestId ? 'border-border bg-accent' : 'border-transparent'}`}>
              <div className="mb-1 flex items-center justify-between gap-2 text-xs text-muted-foreground"><span className="flex items-center gap-2"><Clock className="size-3.5" />{formatTime(item.date || item.createdAt)}</span>{isAdmin && <button type="button" aria-label={`Delete error ${item.requestId}`} title="Delete error" disabled={mutating} onClick={(event) => void removeOne(event, item.requestId)} className="rounded p-1 opacity-100 hover:bg-destructive/10 hover:text-destructive md:opacity-0 md:group-hover:opacity-100"><Trash2 className="size-3" /></button>}</div>
              <div className="truncate font-mono text-xs text-muted-foreground">{item.requestId ? `${item.requestId.slice(0, 8)}…` : '—'}</div>
              <div className="mt-1 truncate text-sm text-destructive" title={item.errorMessage ?? ''}>{item.errorMessage || 'Unknown inference error'}</div>
            </div>)}
            {!loading && errors.length === 0 && <p className="p-8 text-center text-sm italic text-muted-foreground">No errors found.</p>}
          </div>
          {total > 50 && <div className="flex items-center justify-between border-t border-border p-2 text-xs text-muted-foreground"><span>{offset + 1}–{Math.min(offset + 50, total)} of {total}</span><div className="flex gap-1"><Button variant="ghost" size="icon" aria-label="Previous page" disabled={!offset || loading} onClick={() => setOffset(Math.max(0, offset - 50))}><ChevronLeft /></Button><Button variant="ghost" size="icon" aria-label="Next page" disabled={offset + 50 >= total || loading} onClick={() => setOffset(offset + 50)}><ChevronRight /></Button></div></div>}
        </section>

        <section aria-label="Error details" className="relative flex min-h-0 flex-1 flex-col overflow-y-auto bg-background">
          {selected ? <div>
            <div className="border-b border-border p-4">
              <h2 className="mb-3 text-lg font-semibold text-destructive">Error Details</h2>
              <dl className="grid grid-cols-1 gap-2 text-sm sm:grid-cols-2 sm:gap-4">
                <div className="min-w-0"><dt className="inline text-muted-foreground">Request ID:</dt><dd className="ml-2 inline break-all font-mono">{selected.requestId || '—'}</dd></div>
                <div><dt className="inline text-muted-foreground">Time:</dt><dd className="ml-2">{formatTime(selected.date || selected.createdAt)}</dd></div>
              </dl>
            </div>
            <DetailPanel title="Message" value={selected.errorMessage || 'Unknown inference error'} />
            <DetailPanel title="Stack Trace" value={selected.errorStack || '(No stack trace available)'} />
            <DetailPanel title="Additional Details" value={formatDetails(selected.details)} />
          </div> : <div className="flex min-h-64 flex-1 flex-col items-center justify-center gap-4 p-8 text-center text-muted-foreground"><AlertTriangle className="size-12 opacity-20" /><p>{loading ? 'Loading recent errors…' : 'Select an error to inspect details'}</p></div>}
        </section>
      </div>
    </main>
  );
};

const DetailPanel: React.FC<{ title: string; value: string }> = ({ title, value }) => (
  <section className="border-b border-border bg-card">
    <h3 className="bg-muted px-4 py-3 text-xs font-bold uppercase tracking-wider text-muted-foreground">{title}</h3>
    <pre className="max-h-[600px] overflow-auto whitespace-pre-wrap break-words p-4 font-mono text-xs">{value}</pre>
  </section>
);
