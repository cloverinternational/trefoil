// Adapted from Plexus packages/frontend/src/App.tsx, trimmed to the routes
// this vertical slice actually implements (login + one protected page).
import React from 'react';
import { Routes, Route, Navigate, useLocation } from 'react-router-dom';
import { Dashboard } from './pages/Dashboard';
import { Login } from './pages/Login';
import { Providers } from './pages/Providers';
import { ModelAliases } from './pages/ModelAliases';
import { ApiKeys } from './pages/ApiKeys';
import { AuthProvider, useAuth } from './contexts/AuthContext';
import { Quotas } from './pages/Quotas';
import { DetailedUsage } from './pages/DetailedUsage';
import { Configuration } from './pages/Configuration';
import { SystemLogs } from './pages/SystemLogs';
import { McpManagement } from './pages/McpManagement';

const ProtectedRoute: React.FC<{ children: React.ReactNode }> = ({ children }) => {
  const { adminKey } = useAuth();
  const location = useLocation();

  if (!adminKey) {
    return <Navigate to="/login" state={{ from: location }} replace />;
  }
  return <>{children}</>;
};

const AppRoutes = () => (
  <Routes>
    <Route path="/login" element={<Login />} />
    <Route path="/providers" element={<ProtectedRoute><Providers /></ProtectedRoute>} />
    <Route path="/aliases" element={<ProtectedRoute><ModelAliases /></ProtectedRoute>} />
    <Route path="/keys" element={<ProtectedRoute><ApiKeys /></ProtectedRoute>} />
    <Route path="/quotas" element={<ProtectedRoute><Quotas /></ProtectedRoute>} />
    <Route path="/usage" element={<ProtectedRoute><DetailedUsage /></ProtectedRoute>} />
    <Route path="/detailed-usage" element={<ProtectedRoute><DetailedUsage /></ProtectedRoute>} />
    <Route path="/config" element={<ProtectedRoute><Configuration /></ProtectedRoute>} />
    <Route path="/configuration" element={<ProtectedRoute><Configuration /></ProtectedRoute>} />
    <Route path="/system-logs" element={<ProtectedRoute><SystemLogs /></ProtectedRoute>} />
    <Route path="/mcp" element={<ProtectedRoute><McpManagement /></ProtectedRoute>} />
    <Route
      path="/"
      element={
        <ProtectedRoute>
          <Dashboard />
        </ProtectedRoute>
      }
    />
  </Routes>
);

export const App: React.FC = () => (
  <AuthProvider>
    <AppRoutes />
  </AuthProvider>
);
