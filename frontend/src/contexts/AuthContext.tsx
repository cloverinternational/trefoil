// Ported near-verbatim from Plexus packages/frontend/src/contexts/AuthContext.tsx.
// The sequence-guard logic (verifySeqRef) is preserved so a stale mount-time
// verify can't clobber a newer login()/logout() result.
import React, { createContext, useContext, useState, useEffect, useRef } from 'react';
import { verifyAdminKey } from '../lib/api';
import type { Principal } from '../lib/types';

const STORAGE_KEY = 'trefoil_admin_key';

interface AuthContextType {
  adminKey: string | null;
  principal: Principal | null;
  isAuthenticated: boolean;
  isAdmin: boolean;
  isLimited: boolean;
  login: (key: string) => Promise<boolean>;
  logout: () => void;
}

const AuthContext = createContext<AuthContextType | undefined>(undefined);

export const AuthProvider: React.FC<{ children: React.ReactNode }> = ({ children }) => {
  const [adminKey, setAdminKey] = useState<string | null>(null);
  const [principal, setPrincipal] = useState<Principal | null>(null);
  const verifySeqRef = useRef(0);

  useEffect(() => {
    const storedKey = localStorage.getItem(STORAGE_KEY);
    if (!storedKey) return;

    const seq = ++verifySeqRef.current;
    verifyAdminKey(storedKey).then((p) => {
      if (seq !== verifySeqRef.current) return;
      if (p) {
        setAdminKey(storedKey);
        setPrincipal(p);
      } else {
        localStorage.removeItem(STORAGE_KEY);
      }
    });
  }, []);

  const login = async (key: string): Promise<boolean> => {
    const seq = ++verifySeqRef.current;
    const p = await verifyAdminKey(key);
    if (seq !== verifySeqRef.current) return !!p;
    if (p) {
      localStorage.setItem(STORAGE_KEY, key);
      setAdminKey(key);
      setPrincipal(p);
      return true;
    }
    return false;
  };

  const logout = () => {
    verifySeqRef.current += 1;
    localStorage.removeItem(STORAGE_KEY);
    setAdminKey(null);
    setPrincipal(null);
  };

  return (
    <AuthContext.Provider
      value={{
        adminKey,
        principal,
        isAuthenticated: !!adminKey,
        isAdmin: principal?.role === 'admin',
        isLimited: principal?.role === 'limited',
        login,
        logout,
      }}
    >
      {children}
    </AuthContext.Provider>
  );
};

export const useAuth = () => {
  const context = useContext(AuthContext);
  if (context === undefined) {
    throw new Error('useAuth must be used within an AuthProvider');
  }
  return context;
};
