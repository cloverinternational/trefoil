import React, { useEffect, useMemo, useRef, useState } from "react";
import { NavLink, useLocation, useNavigate } from "react-router-dom";
import {
  LayoutDashboard, Server, Boxes, KeyRound, Gauge, ScrollText, AlertTriangle,
  Play, Settings, Plug, Bug, BarChart3, UserRound, Link2, LogOut, Sun, Moon,
  Menu, X, PanelLeftClose, PanelLeftOpen, Search, type LucideIcon,
} from "lucide-react";
import { cn } from "../../lib/utils";
import { useAuth } from "../../contexts/AuthContext";
import { useTheme } from "../../contexts/ThemeContext";
import { Button } from "../ui/button";
import { Dialog, DialogContent, DialogTitle } from "../ui/dialog";
import { TrefoilMark } from "./TrefoilMark";

type Item = { to: string; label: string; icon: LucideIcon; adminOnly?: boolean; key?: string };
const MAIN: Item[] = [
  { key: "o", to: "/", label: "Overview", icon: LayoutDashboard },
  { key: "l", to: "/logs", label: "Logs", icon: ScrollText },
  { key: "e", to: "/errors", label: "Errors", icon: AlertTriangle },
  { key: "p", to: "/playground", label: "Playground", icon: Play },
  { key: "m", to: "/my-key", label: "My key", icon: UserRound },
];
const CONFIG: Item[] = [
  { key: "r", to: "/providers", label: "Providers", icon: Server, adminOnly: true },
  { key: "d", to: "/models", label: "Models", icon: Boxes, adminOnly: true },
  { key: "a", to: "/aliases", label: "Aliases", icon: Link2, adminOnly: true },
  { key: "k", to: "/keys", label: "API keys", icon: KeyRound, adminOnly: true },
  { key: "q", to: "/quotas", label: "Quotas", icon: Gauge, adminOnly: true },
  { key: "u", to: "/usage", label: "Usage", icon: BarChart3 },
  { key: "t", to: "/metrics", label: "Metrics", icon: BarChart3, adminOnly: true },
  { key: "c", to: "/config", label: "Configuration", icon: Settings, adminOnly: true },
  { key: "x", to: "/mcp", label: "MCP", icon: Plug, adminOnly: true },
  { key: "b", to: "/debug", label: "Debug", icon: Bug, adminOnly: true },
  { key: "s", to: "/system-logs", label: "System logs", icon: ScrollText, adminOnly: true },
];

const NavSection: React.FC<{ title: string; items: Item[]; onNavigate: () => void }> = ({
  title, items, onNavigate,
}) => (
  <section>
    <h2 className="eyebrow px-2 pb-1 text-[10px]">
      {title}
    </h2>
    <div>
      {items.map(({ to, label, icon: Icon, key }) => (
        <NavLink
          key={to}
          to={to}
          end={to === "/"}
          onClick={onNavigate}
          className={({ isActive }) =>
            cn(
              "flex items-center gap-2 border-l-2 px-2 py-1 text-xs",
              isActive
                ? "border-primary bg-accent font-semibold text-accent-foreground"
                : "border-transparent text-sidebar-foreground/70 hover:bg-secondary hover:text-sidebar-foreground",
            )
          }
        >
          <Icon className="h-3.5 w-3.5 shrink-0" />
          <span className="flex-1">{label}</span>
          {key && <kbd className="opacity-60">g {key}</kbd>}
        </NavLink>
      ))}
    </div>
  </section>
);

const ShortcutHelp: React.FC<{ open: boolean; onOpenChange: (v: boolean) => void }> = ({ open, onOpenChange }) => (
  <Dialog open={open} onOpenChange={onOpenChange}>
    <DialogContent className="max-w-sm">
      <DialogTitle>Keyboard shortcuts</DialogTitle>
      <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-xs">
        {[["Ctrl/⌘ K", "Command palette"], ["g then …", "Go to page (shown in sidebar)"], ["/", "Focus page search/filter"], ["[", "Collapse sidebar"], ["?", "This help"]].map(([k, d]) => (
          <React.Fragment key={k}><dt><kbd>{k}</kbd></dt><dd className="text-muted-foreground">{d}</dd></React.Fragment>
        ))}
      </dl>
    </DialogContent>
  </Dialog>
);

const Palette: React.FC<{ open: boolean; onOpenChange: (v: boolean) => void; items: Item[] }> = ({ open, onOpenChange, items }) => {
  const navigate = useNavigate();
  const [q, setQ] = useState("");
  const [idx, setIdx] = useState(0);
  const ref = useRef<HTMLInputElement>(null);
  const hits = useMemo(() => items.filter(i => i.label.toLowerCase().includes(q.toLowerCase())), [items, q]);
  useEffect(() => { if (open) { setQ(""); setIdx(0); } }, [open]);
  useEffect(() => setIdx(0), [q]);
  const go = (i?: Item) => { if (i) { navigate(i.to); onOpenChange(false); } };
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-md gap-0 p-0">
        <DialogTitle className="sr-only">Command palette</DialogTitle>
        <input
          ref={ref} autoFocus value={q} onChange={e => setQ(e.target.value)} placeholder="Jump to page…" aria-label="Jump to page"
          onKeyDown={e => {
            if (e.key === "ArrowDown") { e.preventDefault(); setIdx(v => Math.min(v + 1, hits.length - 1)); }
            else if (e.key === "ArrowUp") { e.preventDefault(); setIdx(v => Math.max(v - 1, 0)); }
            else if (e.key === "Enter") go(hits[idx]);
          }}
          className="h-9 border-b bg-transparent px-3 text-xs outline-none"
        />
        <ul role="listbox" className="max-h-72 overflow-y-auto">
          {hits.map((i, n) => (
            <li key={i.to} role="option" aria-selected={n === idx} onMouseEnter={() => setIdx(n)} onClick={() => go(i)}
              className={cn("flex cursor-pointer items-center gap-2 px-3 py-1.5 text-xs", n === idx && "bg-accent text-accent-foreground")}>
              <i.icon className="h-3.5 w-3.5" /><span className="flex-1">{i.label}</span>{i.key && <kbd>g {i.key}</kbd>}
            </li>
          ))}
          {!hits.length && <li className="px-3 py-3 text-xs text-muted-foreground">No matching pages</li>}
        </ul>
      </DialogContent>
    </Dialog>
  );
};

export const AppShell: React.FC<{ children: React.ReactNode }> = ({ children }) => {
  const { isAdmin, logout } = useAuth();
  const { theme, toggle } = useTheme();
  const location = useLocation();
  const navigate = useNavigate();
  const [open, setOpen] = useState(false);
  const [palette, setPalette] = useState(false);
  const [help, setHelp] = useState(false);
  const [collapsed, setCollapsed] = useState(() => localStorage.getItem("trefoil-sidebar-collapsed") === "true");
  const toggleCollapsed = () => setCollapsed(value => {
    localStorage.setItem("trefoil-sidebar-collapsed", String(!value));
    return !value;
  });
  const current = MAIN.concat(CONFIG).find((i) => i.to === location.pathname)?.label ?? (location.pathname.startsWith("/analytics") ? "Analytics" : undefined);
  const filter = (items: Item[]) => items.filter((i) => !i.adminOnly || isAdmin);

  // Power-user keys: Ctrl/Cmd+K palette, "g <key>" go-to, "[" sidebar, "/" focus search, "?" help.
  useEffect(() => {
    let pendingG = 0;
    const onKey = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") { e.preventDefault(); setPalette(v => !v); return; }
      const t = e.target as HTMLElement | null;
      if (e.ctrlKey || e.metaKey || e.altKey || t?.closest("input,textarea,select,[contenteditable=true],[role=dialog]")) return;
      if (pendingG && Date.now() - pendingG < 1200) {
        pendingG = 0;
        const hit = MAIN.concat(CONFIG).find(i => i.key === e.key && (!i.adminOnly || isAdmin));
        if (hit) { e.preventDefault(); navigate(hit.to); }
        return;
      }
      if (e.key === "g") pendingG = Date.now();
      else if (e.key === "[") toggleCollapsed();
      else if (e.key === "?") setHelp(true);
      else if (e.key === "/") {
        const el = document.querySelector<HTMLInputElement>("main input[type=search], main input[placeholder*=earch], main input[placeholder*=ilter]");
        if (el) { e.preventDefault(); el.focus(); }
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }); // re-bound each render so isAdmin/navigate stay current

  const sidebar = (mobile = false) => (
    <aside
      className={cn(
        "fixed inset-y-0 left-0 z-40 flex flex-col border-r border-border bg-sidebar p-2 text-sidebar-foreground",
        mobile ? (open ? "w-56 translate-x-0" : "w-56 -translate-x-full invisible") : cn("hidden lg:flex", collapsed ? "w-11" : "w-52"),
      )}
    >
      <div className="flex items-center justify-between px-1 py-2">
        <div className="flex items-center gap-2">
          <TrefoilMark className="h-5 w-5" />
          {(mobile || !collapsed) && <span className="text-xs font-bold text-sidebar-foreground">Trefoil</span>}
        </div>
        {mobile && (
          <button aria-label="Close navigation" onClick={() => setOpen(false)} className=" p-2 hover:bg-secondary cursor-pointer">
            <X className="h-4 w-4" />
          </button>
        )}
      </div>
      {!mobile && <button type="button" aria-label={collapsed ? "Expand sidebar" : "Collapse sidebar"} onClick={toggleCollapsed} className="mb-2 flex cursor-pointer justify-center p-1.5 hover:bg-secondary">
        {collapsed ? <PanelLeftOpen size={14} /> : <PanelLeftClose size={14} />}
      </button>}
      <nav aria-label="Primary navigation" className="flex-1 space-y-3 overflow-y-auto py-2">
        {collapsed && !mobile ? <>
          {filter(MAIN).concat(filter(CONFIG)).map(({ to, label, icon: Icon }) => <NavLink key={to} to={to} end={to === "/"} title={label} aria-label={label} className={({isActive}) => cn("flex justify-center p-1.5", isActive ? "bg-accent text-accent-foreground" : "text-sidebar-foreground/70 hover:bg-secondary") }><Icon size={14}/></NavLink>)}
        </> : <>
          <NavSection title="Main" items={filter(MAIN)} onNavigate={() => setOpen(false)} />
          <NavSection title="Configuration" items={filter(CONFIG)} onNavigate={() => setOpen(false)} />
        </>}
      </nav>
      <button
        onClick={logout}
        aria-label="Sign out"
        className={cn("flex items-center gap-2 px-2 py-1.5 text-xs text-sidebar-foreground/70 hover:bg-secondary hover:text-sidebar-foreground cursor-pointer", collapsed && !mobile && "justify-center")}
      >
        <LogOut className="h-3.5 w-3.5" />
        {(mobile || !collapsed) && "Sign out"}
      </button>
    </aside>
  );

  return (
    <div className="min-h-screen bg-background">
      {sidebar()}
      {sidebar(true)}
      {open && <div className="fixed inset-0 z-30 bg-black/60 lg:hidden" onClick={() => setOpen(false)} />}
      <header className={cn("sticky top-0 z-20 flex h-9 items-center gap-2 border-b border-border bg-background px-3", collapsed ? "lg:pl-14" : "lg:pl-56")}>
        <Button variant="ghost" size="icon" className="lg:hidden" aria-label="Open navigation" onClick={() => setOpen(true)}>
          <Menu className="h-4 w-4" />
        </Button>
        <h1 className="font-heading text-xs font-semibold uppercase tracking-wide">{current ?? "Trefoil"}</h1>
        <button type="button" onClick={() => setPalette(true)} aria-label="Open command palette" className="ml-auto flex h-6 w-56 cursor-pointer items-center gap-2 border border-input px-2 text-xs text-muted-foreground hover:bg-secondary">
          <Search className="h-3 w-3" /><span className="flex-1 text-left">Jump to page…</span><kbd>Ctrl K</kbd>
        </button>
        <div>
          <Button variant="ghost" size="icon" aria-label="Toggle theme" onClick={toggle}>
            {theme === "dark" ? <Sun className="h-4 w-4" /> : <Moon className="h-4 w-4" />}
          </Button>
        </div>
      </header>
      <main className={cn("", collapsed ? "lg:pl-11" : "lg:pl-52")}>
        <div className="p-3">{children}</div>
        <ShortcutHelp open={help} onOpenChange={setHelp} />
        <Palette open={palette} onOpenChange={setPalette} items={filter(MAIN).concat(filter(CONFIG))} />
      </main>
    </div>
  );
};
