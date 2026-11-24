// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { type ReactElement, useEffect, useMemo, useRef, useState } from 'react';
import { Boxes, CornerDownLeft, Disc3, HardDrive, Rocket, Server, Terminal, Zap, Home } from 'lucide-react';
import { api, navigate, type NavGroup, type Route, type Vm } from '@fluxvm/ui-core';
import { rowsOf } from '../lib/generic';
import { type AppLike, type Item, type Kind, actionItems, appItems, catalogItems, emptyMessage, loadProblem, loadRecent, machineItems, moveSel, ociItems, pageItems, parseQuery, pushRecent, rank, saveRecent, volumeItems, KIND_LABEL } from '../lib/search';

const icon: Record<Kind, ReactElement> = { machine: <Server size={16} />, image: <Disc3 size={16} />, app: <Rocket size={16} />, volume: <HardDrive size={16} />, page: <Home size={16} />, action: <Zap size={16} /> };

interface Loaded { items: Item[]; failed: string[]; loading: boolean }

/** Volumes, images and kube apps, read each time the search opens (machines come from the page's own list). */
function useSearchData(open: boolean): Loaded {
  const [state, setState] = useState<Loaded>({ items: [], failed: [], loading: false });
  useEffect(() => {
    if (!open) return;
    let live = true;
    setState((s) => ({ ...s, loading: true }));
    const apps = async (): Promise<Item[]> => {
      const clusters = (await api.get<{ items?: { name: string }[] }>('/v1/kube')).items ?? [];
      const per = await Promise.all(clusters.slice(0, 8).map(async (c) => appItems(c.name, ((await api.get<{ apps?: AppLike[] }>(`/v1/kube/${encodeURIComponent(c.name)}/apps`).catch(() => ({ apps: [] }))).apps ?? []))));
      return per.flat();
    };
    const sets: [string, () => Promise<Item[]>][] = [
      ['Volumes', async () => volumeItems(rowsOf(await api.get<unknown>('/v1/volumes')))],
      ['Catalog images', async () => catalogItems(rowsOf(await api.get<unknown>('/v1/images/catalog'), 'items'))],
      ['OCI images', async () => ociItems(rowsOf(await api.get<unknown>('/v1/oci/images'), 'items'))],
      ['Apps', apps],
    ];
    void Promise.allSettled(sets.map(([, f]) => f())).then((rs) => {
      if (!live) return;
      const items: Item[] = [];
      const failed: string[] = [];
      rs.forEach((r, i) => (r.status === 'fulfilled' ? items.push(...r.value) : failed.push(loadProblem(sets[i][0], r.reason))));
      setState({ items, failed, loading: false });
    });
    return () => {
      live = false;
    };
  }, [open]);
  return state;
}

/** Cmd/Ctrl-K: find a machine, image, app or volume by name, label, image reference or port; `>` lists actions. */
export function GlobalSearch({ open, onClose, vms, groups, onTheme, onSignOut }: { open: boolean; onClose: () => void; vms: Vm[]; groups: NavGroup[]; onTheme: () => void; onSignOut: () => void }) {
  const [q, setQ] = useState('');
  const [sel, setSel] = useState(0);
  const [recent, setRecent] = useState<string[]>(() => loadRecent());
  const input = useRef<HTMLInputElement>(null);
  const data = useSearchData(open);
  const mode = parseQuery(q);
  const pool = useMemo(
    () => (mode.mode === 'actions' ? actionItems() : [...machineItems(vms.slice(0, 500)), ...data.items, ...pageItems(groups.flatMap((g) => g.items))]),
    [mode.mode, vms, data.items, groups],
  );
  const list = useMemo(() => rank(pool, mode.text, recent), [pool, mode.text, recent]);
  useEffect(() => {
    if (open) {
      setQ('');
      setSel(0);
      setRecent(loadRecent());
      setTimeout(() => input.current?.focus(), 0);
    }
  }, [open]);
  useEffect(() => setSel(0), [q]);
  useEffect(() => document.getElementById('fx-search-sel')?.scrollIntoView?.({ block: 'nearest' }), [sel, list]);
  if (!open) return null;

  const pick = (r?: { item: Item }) => {
    if (!r) return;
    const it = r.item;
    onClose();
    if (it.kind === 'action') {
      if (it.id === 'action:theme') return onTheme();
      if (it.id === 'action:signout') return onSignOut();
    } else {
      const next = pushRecent(recent, it.id);
      setRecent(next);
      saveRecent(next);
    }
    if (it.to) navigate(it.to);
  };
  const cur = list[sel]?.item;
  return (
    <div className="fx-palette-wrap" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="fx-palette" role="dialog" aria-modal="true" aria-label="Search">
        <input
          ref={input}
          value={q}
          role="combobox"
          aria-expanded="true"
          aria-controls="fx-search-list"
          aria-activedescendant={cur ? 'fx-search-sel' : undefined}
          placeholder="Search machines, images, apps, volumes. Type > for actions"
          onChange={(e) => setQ(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'ArrowDown') (e.preventDefault(), setSel((s) => moveSel(s, 1, list.length)));
            else if (e.key === 'ArrowUp') (e.preventDefault(), setSel((s) => moveSel(s, -1, list.length)));
            else if (e.key === 'Enter') pick(list[sel]);
            else if (e.key === 'Escape') onClose();
          }}
        />
        <ul id="fx-search-list" role="listbox">
          {data.loading && mode.mode === 'search' ? <li className="fx-palette-none">Looking through volumes, images and apps…</li> : null}
          {!list.length ? <li className="fx-palette-none">{emptyMessage(mode.text, mode.mode, mode.mode === 'search' ? data.failed : [])}</li> : null}
          {list.map((r, i) => (
            <li key={r.item.id} id={i === sel ? 'fx-search-sel' : undefined} role="option" aria-selected={i === sel} className={i === sel ? 'is-sel' : ''} onMouseEnter={() => setSel(i)} onClick={() => pick(r)}>
              <span className="fx-palette-ico">{icon[r.item.kind]}</span>
              <span className="fx-palette-label">{r.item.title}{r.item.sub ? <small className="fx-search-sub">{r.item.sub}</small> : null}</span>
              <small>{r.recent && !mode.text ? 'Recent' : KIND_LABEL[r.item.kind]}</small>
              {i === sel ? <CornerDownLeft size={13} /> : null}
            </li>
          ))}
        </ul>
        <div className="fx-search-foot">
          {cur?.cmd ? <><Terminal size={13} /> <code>{cur.cmd}</code></> : cur?.kind === 'action' ? <>Studio only: there is no fluxctl command for this.</> : <><Boxes size={13} /> Enter opens it. Up and down move, Esc closes.</>}
        </div>
      </div>
    </div>
  );
}

/** When a route carries `focus=NAME`, scroll to the row that shows NAME and flash it (the page may still be loading). */
export function useFocusFromRoute(route: Route): void {
  const focus = route.query.get('focus') ?? '';
  useEffect(() => {
    if (!focus) return;
    let tries = 0;
    let marked: Element | null = null;
    const timer = setInterval(() => {
      const hit = [...document.querySelectorAll('main td, main li, main h2, main h3, main .vm-name')].find((el) => el.children.length < 3 && el.textContent?.trim() === focus);
      const row = hit?.closest('tr, li, .fx-card') ?? hit;
      if (row) {
        marked = row;
        row.classList.add('fx-search-hit');
        row.scrollIntoView?.({ block: 'center', behavior: 'smooth' });
        clearInterval(timer);
        setTimeout(() => marked?.classList.remove('fx-search-hit'), 2600);
      } else if (++tries > 20) clearInterval(timer);
    }, 200);
    return () => {
      clearInterval(timer);
      marked?.classList.remove('fx-search-hit');
    };
  }, [route.path, focus]);
}
