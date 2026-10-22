import { useCallback, useEffect, useMemo, useState, type ReactNode } from 'react';
import { Archive, Bot, Boxes, LayoutDashboard, LogOut, Moon, Network, Server } from 'lucide-react';
import {
  CommandPalette, GlobalNav, LoginCard, Toasts, api, navigate, onUnauthorized, setToken, toggleTheme,
  useRoute, type Command, type NavGroup, type ToastItem, type Vm,
} from '@fluxvm/ui-core';
import { NAV } from './nav';
import { Overview } from './pages/Overview';
import { Machines } from './pages/Machines';
import { BackupsPage, NetworkPage } from './pages/NodePages';
import { AgentDetail, AgentsPage } from './pages/Agents';
import { SandboxDetail, SandboxesPage } from './pages/Sandboxes';
import { MachineDetail } from './pages/MachineDetail';

export type Notify = (text: string, tone?: 'ok' | 'bad') => void;

const icons: Record<string, ReactNode> = { '/': <LayoutDashboard size={16} />, '/machines': <Server size={16} />, '/network': <Network size={16} />, '/backups': <Archive size={16} />, '/sandboxes': <Boxes size={16} />, '/agents': <Bot size={16} /> };

export function App() {
  const route = useRoute();
  const [auth, setAuth] = useState<'checking' | 'in' | 'out'>('checking');
  const [palette, setPalette] = useState(false);
  const [toasts, setToasts] = useState<ToastItem[]>([]);
  const [vms, setVms] = useState<Vm[]>([]);

  const notify = useCallback<Notify>((text, tone = 'ok') => {
    const id = Date.now() + Math.random();
    setToasts((t) => [...t, { id, text, tone }]);
    setTimeout(() => setToasts((t) => t.filter((x) => x.id !== id)), 4200);
  }, []);

  useEffect(() => onUnauthorized(() => setAuth('out')), []);
  useEffect(() => {
    api
      .get<{ items: Vm[] }>('/v1/vms')
      .then((r) => (setVms(r.items ?? []), setAuth('in')))
      .catch(() => setAuth((a) => (a === 'checking' ? 'out' : a)));
  }, []);
  useEffect(() => {
    const f = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
        e.preventDefault();
        setPalette((p) => !p);
      }
    };
    window.addEventListener('keydown', f);
    return () => window.removeEventListener('keydown', f);
  }, []);

  const groups: NavGroup[] = useMemo(() => NAV.map((g) => ({ ...g, items: g.items.map((i) => ({ ...i, icon: icons[i.to] })) })), []);
  const commands: Command[] = useMemo(
    () => [
      ...groups.flatMap((g) => g.items.map((i) => ({ id: 'nav' + i.to, label: `Go to ${i.label}`, group: 'Navigate', icon: i.icon, run: () => navigate(i.to) }))),
      ...vms.slice(0, 200).map((v) => ({ id: 'vm' + v.id, label: v.name, group: 'Machines', hint: v.status, icon: <Server size={16} />, run: () => navigate('/machines?q=' + encodeURIComponent(v.name)) })),
      { id: 'theme', label: 'Toggle light / dark theme', group: 'Appearance', icon: <Moon size={16} />, run: () => void toggleTheme() },
      { id: 'out', label: 'Sign out', group: 'Session', icon: <LogOut size={16} />, run: () => (setToken(''), setAuth('out')) },
    ],
    [groups, vms],
  );

  if (auth === 'checking') return null;
  if (auth === 'out')
    return (
      <LoginCard
        logo={<span className="fx-logo lg" />}
        product="FluxVM Studio"
        blurb="Every machine, sandbox and agent on this node — in one calm place."
        onSignedIn={() => (setAuth('in'), navigate('/'))}
      />
    );

  const detailId = route.segs[0] === 'machines' && route.segs[1] ? route.segs[1] : '';
  const subId = route.segs[1] ?? '';
  const page = detailId ? <MachineDetail id={detailId} notify={notify} /> : route.segs[0] === 'sandboxes' && subId ? <SandboxDetail id={subId} notify={notify} /> : route.segs[0] === 'agents' && subId ? <AgentDetail id={subId} notify={notify} /> : route.path === '/sandboxes' ? <SandboxesPage notify={notify} /> : route.path === '/agents' ? <AgentsPage notify={notify} /> : route.path === '/' ? <Overview vms={vms} setVms={setVms} /> : route.path === '/network' ? <NetworkPage notify={notify} /> : route.path === '/backups' ? <BackupsPage notify={notify} /> : route.path.startsWith('/machines') ? <Machines vms={vms} setVms={setVms} notify={notify} q={route.query.get('q') ?? ''} /> : <Overview vms={vms} setVms={setVms} />;
  return (
    <>
      <GlobalNav
        brand={<><span className="fx-logo" /> FluxVM</>}
        groups={groups}
        active={route.path}
        onPalette={() => setPalette(true)}
        right={<button className="fx-icon-btn" aria-label="Sign out" onClick={() => (setToken(''), setAuth('out'))}><LogOut size={16} /></button>}
      />
      <main className="fx-page">{page}</main>
      <CommandPalette open={palette} onClose={() => setPalette(false)} commands={commands} />
      <Toasts items={toasts} />
    </>
  );
}
