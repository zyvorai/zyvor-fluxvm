import { useCallback, useEffect, useMemo, useState, type ReactNode } from 'react';
import { Activity, Cable, CreditCard, Cpu, Flame, HardDrive, KeyRound, LayoutDashboard, Layers, LogOut, Moon, Network, Server, ShieldCheck, SlidersHorizontal, Users } from 'lucide-react';
import {
  ApiError, CommandPalette, GlobalNav, LoginCard, Toasts, api, navigate, onUnauthorized, setDefaultHeaders, setToken, toggleTheme,
  useRoute, type Command, type NavGroup, type ToastItem,
} from '@fluxvm/ui-core';
import { navFor } from './nav';
import type { Role } from './lib/fleet';
import { Overview } from './pages/Overview';
import { HostsPage } from './pages/Hosts';
import { VmsPage } from './pages/Vms';
import { NetworksPage } from './pages/Networks';
import { TenantsPage } from './pages/Tenants';
import { KeysPage } from './pages/Keys';
import { BillingPage } from './pages/Billing';
import { ActivityPage } from './pages/Activity';
import { VolumesPage } from './pages/Volumes';
import { TunnelsPage } from './pages/Tunnels';
import { PoolsPage } from './pages/Pools';
import { EnrollmentPage } from './pages/Enrollment';
import { OperationsPage } from './pages/Operations';
import { ImagesPage } from './pages/Images';

export type Notify = (text: string, tone?: 'ok' | 'bad') => void;
export interface Ctx { role: Role; notify: Notify }

// Central refuses changes made with a cookie session unless this header is present.
setDefaultHeaders({ 'X-FluxVM-Console': '1' });

const icons: Record<string, ReactNode> = { '/': <LayoutDashboard size={16} />, '/hosts': <Cpu size={16} />, '/vms': <Server size={16} />, '/networks': <Network size={16} />, '/volumes': <HardDrive size={16} />, '/pools': <Flame size={16} />, '/tunnels': <Cable size={16} />, '/images': <Layers size={16} />, '/enrollment': <ShieldCheck size={16} />, '/operations': <SlidersHorizontal size={16} />, '/tenants': <Users size={16} />, '/keys': <KeyRound size={16} />, '/billing': <CreditCard size={16} />, '/activity': <Activity size={16} /> };

/** `/fleet/tenants/me` answers 200 for a tenant and 400 ("not a tenant") for the operator. */
async function detectRole(): Promise<Role> {
  try {
    await api.get('/fleet/tenants/me');
    return 'tenant';
  } catch (e) {
    if (e instanceof ApiError && e.status === 400) return 'operator';
    throw e;
  }
}

export function App() {
  const route = useRoute();
  const [auth, setAuth] = useState<'checking' | 'in' | 'out'>('checking');
  const [role, setRole] = useState<Role>('operator');
  const [sso, setSso] = useState<{ label: string; href: string } | undefined>();
  const [palette, setPalette] = useState(false);
  const [toasts, setToasts] = useState<ToastItem[]>([]);

  const notify = useCallback<Notify>((text, tone = 'ok') => {
    const id = Date.now() + Math.random();
    setToasts((t) => [...t, { id, text, tone }]);
    setTimeout(() => setToasts((t) => t.filter((x) => x.id !== id)), 4200);
  }, []);

  const enter = useCallback(async () => {
    try {
      setRole(await detectRole());
      setAuth('in');
    } catch {
      setAuth('out');
    }
  }, []);
  useEffect(() => onUnauthorized(() => setAuth('out')), []);
  useEffect(() => {
    void enter();
    api.get<{ enabled?: boolean; provider?: string }>('/auth/config').then((c) => c?.enabled && setSso({ label: `Sign in with ${c.provider || 'SSO'}`, href: '/auth/login' }), () => undefined);
  }, [enter]);
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

  const groups: NavGroup[] = useMemo(() => navFor(role).map((g) => ({ ...g, items: g.items.map((i) => ({ ...i, icon: icons[i.to] })) })), [role]);
  const signOut = async () => {
    await api.post('/auth/logout').catch(() => undefined);
    setToken('');
    setAuth('out');
  };
  const commands: Command[] = useMemo(
    () => [
      ...groups.flatMap((g) => g.items.map((i) => ({ id: 'nav' + i.to, label: `Go to ${i.label}`, group: 'Navigate', icon: i.icon, run: () => navigate(i.to) }))),
      { id: 'theme', label: 'Toggle light / dark theme', group: 'Appearance', icon: <Moon size={16} />, run: () => void toggleTheme() },
      { id: 'out', label: 'Sign out', group: 'Session', icon: <LogOut size={16} />, run: () => void signOut() },
    ],
    [groups],
  );

  if (auth === 'checking') return null;
  if (auth === 'out')
    return <LoginCard logo={<span className="fx-logo lg" />} product="FluxVM Cloud" blurb="Hosts, machines and tenants across your whole fleet." probe="/fleet/leases" sso={sso} onSignedIn={() => { navigate('/'); void enter(); }} />;

  const ctx: Ctx = { role, notify };
  const page =
    route.path === '/hosts' ? <HostsPage {...ctx} /> :
    route.path === '/vms' ? <VmsPage {...ctx} /> :
    route.path === '/networks' ? <NetworksPage {...ctx} /> :
    route.path === '/volumes' ? <VolumesPage {...ctx} /> :
    route.path === '/pools' ? <PoolsPage {...ctx} /> :
    route.path === '/tunnels' ? <TunnelsPage {...ctx} /> :
    route.path === '/images' && role === 'operator' ? <ImagesPage {...ctx} /> :
    route.path === '/enrollment' && role === 'operator' ? <EnrollmentPage {...ctx} /> :
    route.path === '/operations' && role === 'operator' ? <OperationsPage {...ctx} /> :
    route.path === '/tenants' && role === 'operator' ? <TenantsPage {...ctx} /> :
    route.path === '/keys' && role === 'tenant' ? <KeysPage {...ctx} /> :
    route.path === '/billing' ? <BillingPage {...ctx} /> :
    route.path === '/activity' ? <ActivityPage {...ctx} /> :
    <Overview {...ctx} />;
  return (
    <>
      <GlobalNav
        brand={<><span className="fx-logo" /> FluxVM Cloud</>}
        groups={groups}
        active={route.path}
        onPalette={() => setPalette(true)}
        right={<button className="fx-icon-btn" aria-label="Sign out" onClick={() => void signOut()}><LogOut size={16} /></button>}
      />
      <main className="fx-page">{page}</main>
      <CommandPalette open={palette} onClose={() => setPalette(false)} commands={commands} />
      <Toasts items={toasts} />
    </>
  );
}
