// Adapted from Plexus packages/frontend/src/pages/Login.tsx. Dropped the
// OAuth returnTo/token query-param handling since Trefoil's backend doesn't
// implement OAuth login yet; kept the credential form, show/hide toggle,
// and error handling wired to the same verify contract.
import React, { useState, useEffect } from 'react';
import { useNavigate, useLocation } from 'react-router-dom';
import { ArrowRight, AlertCircle, Eye, EyeOff, KeyRound, ShieldCheck } from 'lucide-react';
import { useAuth } from '../contexts/AuthContext';
import { Button } from '../components/ui/Button';
import { TrefoilMark } from '../components/layout/TrefoilMark';

export const Login: React.FC = () => {
  const [key, setKey] = useState('');
  const [showKey, setShowKey] = useState(false);
  const { login, isAuthenticated } = useAuth();
  const [error, setError] = useState('');
  const navigate = useNavigate();
  const location = useLocation();

  const from = (location.state as { from?: { pathname?: string } } | null)?.from?.pathname || '/';

  useEffect(() => {
    if (isAuthenticated) {
      navigate(from, { replace: true });
    }
  }, [isAuthenticated, navigate, from]);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!key.trim()) {
      setError('Please enter a key');
      return;
    }
    const valid = await login(key.trim());
    if (!valid) {
      setError('Invalid key. Verify the prefix and try again.');
    }
  };

  return (
    <div className="min-h-screen flex items-center justify-center bg-bg-deep p-4 sm:p-6">
      <main className="relative w-full max-w-md">
        <div className="mb-6 flex flex-wrap items-center justify-center gap-3 sm:mb-8">
          <div className="animate-float">
            <TrefoilMark size={44} />
          </div>
          <div className="flex items-baseline gap-2">
            <span className="text-3xl font-bold amber-grad-text font-heading tracking-tight">
              Trefoil
            </span>
          </div>
        </div>

        <div className="glass-bg rounded-xl p-5 shadow-2xl sm:rounded-2xl sm:p-8">
          <div className="mb-6">
            <h1 className="font-heading text-2xl font-semibold tracking-tight mb-1.5">Sign in</h1>
            <p className="text-sm text-text-secondary leading-relaxed">
              Enter your admin key for full access, or an API key secret for a scoped view of your
              key's activity.
            </p>
          </div>

          <form onSubmit={handleSubmit} className="space-y-4" noValidate>
            <div>
              <label
                htmlFor="adminKey"
                className="block text-xs font-medium text-text-secondary mb-1.5 uppercase tracking-wider"
              >
                Admin key or API key secret
              </label>
              <div className="relative">
                <KeyRound
                  size={16}
                  className="absolute left-3 top-1/2 -translate-y-1/2 text-text-muted pointer-events-none"
                />
                <input
                  id="adminKey"
                  type={showKey ? 'text' : 'password'}
                  autoComplete="current-password"
                  value={key}
                  onChange={(e) => {
                    setKey(e.target.value);
                    if (error) setError('');
                  }}
                  placeholder="sk-admin-•••• or sk-•••••••••••••"
                  autoFocus
                  className="w-full bg-slate-900/60 border border-border rounded-md py-3 pl-10 pr-10 text-text font-mono text-sm placeholder:text-text-muted hover:border-border-2 focus:border-primary focus:shadow-[0_0_0_3px_rgba(245,158,11,0.18)] focus:outline-none transition-all duration-fast"
                />
                <button
                  type="button"
                  onClick={() => setShowKey((v) => !v)}
                  className="absolute right-2 top-1/2 -translate-y-1/2 p-1.5 rounded-md hover:bg-slate-700/50 text-text-secondary"
                  aria-label={showKey ? 'Hide key' : 'Show key'}
                >
                  {showKey ? <EyeOff size={14} /> : <Eye size={14} />}
                </button>
              </div>
              {error && (
                <div className="mt-2 flex items-start gap-2 text-xs text-rose-300 bg-rose-500/10 border border-rose-500/30 rounded-lg p-2.5">
                  <AlertCircle size={14} className="mt-0.5 flex-none" />
                  <span>{error}</span>
                </div>
              )}
            </div>

            <Button type="submit" variant="primary" className="w-full py-3 text-sm font-semibold">
              <span>Access Dashboard</span>
              <ArrowRight size={14} />
            </Button>
          </form>

          <div className="mt-6 flex flex-col gap-2 border-t border-white/5 pt-5 text-[11px] text-text-muted sm:flex-row sm:items-center sm:justify-between">
            <span className="inline-flex items-center gap-1.5">
              <ShieldCheck size={14} />
              End-to-end encrypted
            </span>
          </div>
        </div>
      </main>
    </div>
  );
};
