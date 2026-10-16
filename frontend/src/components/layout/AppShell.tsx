import React, { useState } from "react";
import { NavLink, useLocation } from "react-router-dom";
import {
  LayoutDashboard, Server, Boxes, KeyRound, Gauge, ScrollText, AlertTriangle,
  Play, Settings, Plug, Bug, BarChart3, UserRound, Link2, LogOut, Sun, Moon,
  Menu, X, PanelLeftClose, PanelLeftOpen, type LucideIcon,
} from "lucide-react";
import { cn } from "../../lib/utils";
import { useAuth } from "../../contexts/AuthContext";
import { useTheme } from "../../contexts/ThemeContext";
import { Button } from "../ui/button";
import { TrefoilMark } from "./TrefoilMark";

type Item = { to: string; label: string; icon: LucideIcon; adminOnly?: boolean };
const MAIN: Item[] = [
  { to: "/", label: "Overview", icon: LayoutDashboard },
  { to: "/logs", label: "Logs", icon: ScrollText },
  { to: "/errors", label: "Errors", icon: AlertTriangle },
  { to: "/playground", label: "Playground", icon: Play },
  { to: "/my-key", label: "My key", icon: UserRound },
];
const CONFIG: Item[] = [
  { to: "/providers", label: "Providers", icon: Server, adminOnly: true },
  { to: "/models", label: "Models", icon: Boxes, adminOnly: true },
  { to: "/aliases", label: "Aliases", icon: Link2, adminOnly: true },
  { to: "/keys", label: "API keys", icon: KeyRound, adminOnly: true },
  { to: "/quotas", label: "Quotas", icon: Gauge, adminOnly: true },
  { to: "/usage", label: "Usage", icon: BarChart3 },
  { to: "/metrics", label: "Metrics", icon: BarChart3, adminOnly: true },
  { to: "/config", label: "Configuration", icon: Settings, adminOnly: true },
  { to: "/mcp", label: "MCP", icon: Plug, adminOnly: true },
  { to: "/debug", label: "Debug", icon: Bug, adminOnly: true },
  { to: "/system-logs", label: "System logs", icon: ScrollText, adminOnly: true },
];

const NavSection: React.FC<{ title: string; items: Item[]; onNavigate: () => void }> = ({
  title, items, onNavigate,
}) => (
  <section>
    <h2 className="eyebrow px-3 pb-2">
      {title}
    </h2>
    <div className="space-y-1">
      {items.map(({ to, label, icon: Icon }) => (
        <NavLink
          key={to}
          to={to}
          end={to === "/"}
          onClick={onNavigate}
          className={({ isActive }) =>
            cn(
              "flex items-center gap-3 rounded-lg px-3 py-1.5 text-sm transition-colors",
              isActive
                ? "bg-accent font-semibold text-accent-foreground"
                : "text-sidebar-foreground/70 hover:bg-secondary hover:text-sidebar-foreground",
            )
          }
        >
          <Icon className="h-4 w-4 shrink-0" />
          {label}
        </NavLink>
      ))}
    </div>
  </section>
);

export const AppShell: React.FC<{ children: React.ReactNode }> = ({ children }) => {
  const { isAdmin, logout } = useAuth();
  const { theme, toggle } = useTheme();
  const location = useLocation();
  const [open, setOpen] = useState(false);
  const [collapsed, setCollapsed] = useState(() => localStorage.getItem("trefoil-sidebar-collapsed") === "true");
  const toggleCollapsed = () => setCollapsed(value => {
    localStorage.setItem("trefoil-sidebar-collapsed", String(!value));
    return !value;
  });
  const current = MAIN.concat(CONFIG).find((i) => i.to === location.pathname)?.label;
  const filter = (items: Item[]) => items.filter((i) => !i.adminOnly || isAdmin);

  const sidebar = (mobile = false) => (
    <aside
      className={cn(
        "fixed inset-y-0 left-0 z-40 flex flex-col border-r border-border bg-sidebar p-3 text-sidebar-foreground transition-[transform,width] duration-300",
        mobile ? (open ? "w-64 translate-x-0" : "w-64 -translate-x-full invisible") : cn("hidden lg:flex", collapsed ? "w-16" : "w-64"),
      )}
    >
      <div className="flex items-center justify-between px-2 py-3">
        <div className="flex items-center gap-2">
          <TrefoilMark className="h-6 w-6" />
          {(mobile || !collapsed) && <span className="font-heading text-lg font-bold text-sidebar-foreground">Trefoil</span>}
        </div>
        {mobile && (
          <button aria-label="Close navigation" onClick={() => setOpen(false)} className="rounded p-2 hover:bg-secondary cursor-pointer">
            <X className="h-4 w-4" />
          </button>
        )}
      </div>
      {!mobile && <button type="button" aria-label={collapsed ? "Expand sidebar" : "Collapse sidebar"} onClick={toggleCollapsed} className="mb-3 flex cursor-pointer justify-center rounded-lg p-2 hover:bg-secondary">
        {collapsed ? <PanelLeftOpen size={18} /> : <PanelLeftClose size={18} />}
      </button>}
      <nav aria-label="Primary navigation" className="flex-1 space-y-6 overflow-y-auto py-2">
        {collapsed && !mobile ? <>
          {filter(MAIN).concat(filter(CONFIG)).map(({ to, label, icon: Icon }) => <NavLink key={to} to={to} end={to === "/"} title={label} aria-label={label} className={({isActive}) => cn("flex justify-center rounded-lg p-2", isActive ? "bg-accent text-accent-foreground" : "text-sidebar-foreground/70 hover:bg-secondary") }><Icon size={18}/></NavLink>)}
        </> : <>
          <NavSection title="Main" items={filter(MAIN)} onNavigate={() => setOpen(false)} />
          <NavSection title="Configuration" items={filter(CONFIG)} onNavigate={() => setOpen(false)} />
        </>}
      </nav>
      <button
        onClick={logout}
        aria-label="Sign out"
        className={cn("flex items-center gap-3 rounded-lg px-3 py-2 text-sm text-sidebar-foreground/70 hover:bg-secondary hover:text-sidebar-foreground cursor-pointer", collapsed && !mobile && "justify-center")}
      >
        <LogOut className="h-4 w-4" />
        {(mobile || !collapsed) && "Sign out"}
      </button>
    </aside>
  );

  return (
    <div className="min-h-screen bg-background">
      {sidebar()}
      {sidebar(true)}
      {open && <div className="fixed inset-0 z-30 bg-black/60 lg:hidden" onClick={() => setOpen(false)} />}
      <header className={cn("sticky top-0 z-20 flex h-14 items-center gap-3 border-b border-border bg-background/90 px-4 backdrop-blur transition-[padding] duration-300", collapsed ? "lg:pl-20" : "lg:pl-72")}>
        <Button variant="ghost" size="icon" className="lg:hidden" aria-label="Open navigation" onClick={() => setOpen(true)}>
          <Menu className="h-5 w-5" />
        </Button>
        <h1 className="font-heading text-sm font-semibold">{current ?? "Trefoil"}</h1>
        <div className="ml-auto">
          <Button variant="ghost" size="icon" aria-label="Toggle theme" onClick={toggle}>
            {theme === "dark" ? <Sun className="h-4 w-4" /> : <Moon className="h-4 w-4" />}
          </Button>
        </div>
      </header>
      <main className={cn("transition-[padding] duration-300", collapsed ? "lg:pl-16" : "lg:pl-64")}>
        <div className="mx-auto max-w-7xl p-4 md:p-6">{children}</div>
      </main>
    </div>
  );
};
