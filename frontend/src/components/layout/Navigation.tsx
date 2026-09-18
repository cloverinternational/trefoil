import React from 'react';
import { NavLink } from 'react-router-dom';
import { useAuth } from '../../contexts/AuthContext';

const links = [['/', 'Overview', false], ['/providers', 'Providers', true], ['/models', 'Models', true], ['/aliases', 'Aliases', true], ['/keys', 'API keys', true], ['/quotas', 'Quotas', true], ['/usage', 'Usage', false], ['/logs', 'Logs', false], ['/errors', 'Errors', false], ['/playground', 'Playground', false], ['/my-key', 'My key', false], ['/metrics', 'Metrics', true], ['/config', 'Configuration', true], ['/mcp', 'MCP', true], ['/debug', 'Debug', true], ['/system-logs', 'System logs', true]] as const;

export const Navigation: React.FC = () => {
  const { isAdmin } = useAuth();
  return <nav aria-label="Primary navigation" className="-mx-1 overflow-x-auto border-b border-border pb-3"><div className="flex min-w-max items-center gap-1">{links.filter(([, , adminOnly]) => !adminOnly || isAdmin).map(([to, label]) => <NavLink key={to} to={to} end={to === '/'} className={({ isActive }) => `rounded-md px-2.5 py-1.5 text-xs font-medium transition ${isActive ? 'bg-primary/15 text-primary' : 'text-text-secondary hover:bg-slate-800 hover:text-text'}`}>{label}</NavLink>)}</div></nav>;
};
