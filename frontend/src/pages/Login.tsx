import React, { useEffect, useState } from 'react';
import { useLocation, useNavigate } from 'react-router-dom';
import { AlertCircle, ArrowRight, Eye, EyeOff, KeyRound, ShieldCheck } from 'lucide-react';
import { useAuth } from '../contexts/AuthContext';
import { TrefoilMark } from '../components/layout/TrefoilMark';
import { Button } from '../components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '../components/ui/card';
import { Input } from '../components/ui/input';
import { Label } from '../components/ui/label';

export const Login: React.FC = () => {
  const [key, setKey] = useState('');
  const [showKey, setShowKey] = useState(false);
  const [error, setError] = useState('');
  const { login, isAuthenticated } = useAuth();
  const navigate = useNavigate();
  const location = useLocation();
  const destination = (location.state as { from?: { pathname?: string; search?: string } } | null)?.from;
  const from = `${destination?.pathname || '/'}${destination?.search || ''}`;

  useEffect(() => {
    if (isAuthenticated) navigate(from, { replace: true });
  }, [isAuthenticated, navigate, from]);

  const handleSubmit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!key.trim()) { setError('Please enter a key'); return; }
    const valid = await login(key.trim());
    if (!valid) setError('Invalid key. Verify the prefix and try again.');
  };

  return (
    <main className="flex min-h-screen items-center justify-center bg-background p-4">
      <div className="w-full max-w-md">
        <div className="mb-6 flex items-center justify-center gap-3">
          <TrefoilMark size={42} />
          <span className="font-heading text-base font-bold tracking-tight">Trefoil</span>
        </div>
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Sign in</CardTitle>
            <CardDescription>Enter your admin key for full access, or an API key secret for a scoped view of your key&apos;s activity.</CardDescription>
          </CardHeader>
          <CardContent>
            <form onSubmit={handleSubmit} className="space-y-3" noValidate>
              <div className="space-y-2">
                <Label htmlFor="adminKey">Admin key or API key secret</Label>
                <div className="relative">
                  <KeyRound className="absolute left-3 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />
                  <Input id="adminKey" type={showKey ? 'text' : 'password'} autoComplete="current-password" autoFocus value={key} onChange={event => { setKey(event.target.value); if (error) setError(''); }} placeholder="sk-admin-•••• or sk-•••••••••••••" className="pr-20 pl-10 font-mono" />
                  <Button type="button" variant="ghost" size="sm" className="absolute right-1 top-1/2 -translate-y-1/2" aria-label={showKey ? 'Hide key' : 'Show key'} onClick={() => setShowKey(value => !value)}>{showKey ? <EyeOff /> : <Eye />}</Button>
                </div>
                {error && <div role="alert" className="flex items-start gap-2  border border-destructive/30 bg-destructive/10 p-3 text-xs text-destructive"><AlertCircle className="mt-0.5 size-4 shrink-0" /><span>{error}</span></div>}
              </div>
              <Button type="submit" className="w-full">Access Dashboard <ArrowRight /></Button>
            </form>
            <div className="mt-6 flex items-center gap-2 border-t border-border pt-4 text-xs text-muted-foreground"><ShieldCheck className="size-4" /> End-to-end encrypted</div>
          </CardContent>
        </Card>
      </div>
    </main>
  );
};
