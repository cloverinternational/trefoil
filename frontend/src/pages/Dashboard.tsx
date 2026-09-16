import React from 'react';
import { useAuth } from '../contexts/AuthContext';

export const Dashboard: React.FC = () => {
  const { principal, logout } = useAuth();
  return (
    <div className="min-h-screen bg-bg-deep text-text p-8">
      <h1 className="text-2xl font-heading font-semibold mb-4">Trefoil</h1>
      <p className="text-text-secondary mb-6">
        Signed in as {principal?.role === 'admin' ? 'admin' : `limited (${principal?.keyName})`}
      </p>
      <button
        onClick={logout}
        className="text-sm text-text-secondary underline hover:text-text"
      >
        Log out
      </button>
    </div>
  );
};
