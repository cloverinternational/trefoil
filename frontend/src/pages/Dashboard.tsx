import React from 'react';
import { useEffect, useState } from 'react';
import { useAuth } from '../contexts/AuthContext';
import { getConfigStatus, getUsageSummary } from '../lib/api';

export const Dashboard: React.FC = () => {
  const { principal, logout } = useAuth();
  const [status, setStatus] = useState<Awaited<ReturnType<typeof getConfigStatus>> | null>(null);
  const [summary, setSummary] = useState<Record<string, number> | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    Promise.all([getConfigStatus(), getUsageSummary()]).then(([config, usage]) => {
      setStatus(config);
      setSummary(usage);
    }).catch((reason: Error) => setError(reason.message));
  }, []);
  return (
    <div className="min-h-screen bg-bg-deep text-text p-8">
      <h1 className="text-2xl font-heading font-semibold mb-4">Trefoil</h1>
      <p className="text-text-secondary mb-6">
        Signed in as {principal?.role === 'admin' ? 'admin' : `limited (${principal?.keyName})`}
      </p>
      {error && <p className="mb-4 text-red-400">{error}</p>}
      <div className="grid grid-cols-2 gap-4 max-w-xl">
        {[['Providers', status?.providerCount], ['Aliases', status?.modelAliasCount], ['API keys', status?.keyCount], ['Requests', summary?.requests]].map(([label, value]) => (
          <div key={label} className="rounded-lg border border-border bg-bg-surface p-4">
            <div className="text-sm text-text-secondary">{label}</div>
            <div className="mt-2 text-2xl font-semibold">{value ?? '—'}</div>
          </div>
        ))}
      </div>
      <button
        onClick={logout}
        className="text-sm text-text-secondary underline hover:text-text"
      >
        Log out
      </button>
    </div>
  );
};
