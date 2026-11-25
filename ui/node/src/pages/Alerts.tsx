// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { useCallback, useEffect, useRef, useState } from 'react';
import { Bell } from 'lucide-react';
import { Button, Card, EmptyState, Hero, Pill, Skeleton, api, navigate } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { CopyCmd } from './Images';
import { ALERTS_CHANGED, SNOOZE_HOURS, WINDOWS, ago, alertsCommand, alertsPath, badge, changeFailed, emptyWords, errorWords, isSnoozed, levelTone, snoozeBody, snoozeCommand, unsnoozeCommand, type Alert, type AlertsReply } from '../lib/alerts';

const POLL_MS = 30000;

/** The reply, refreshed every 30 seconds while the page is open. `null` until the first answer; `error` when it cannot be read. */
export function useAlerts(includeSnoozed = false, since?: string): { reply: AlertsReply | null; error: string | null; reload: () => Promise<void> } {
  const [reply, setReply] = useState<AlertsReply | null>(null);
  const [error, setError] = useState<string | null>(null);
  const reload = useCallback(async () => {
    try {
      setReply(await api.get<AlertsReply>(alertsPath({ includeSnoozed, since })));
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    }
  }, [includeSnoozed, since]);
  useEffect(() => {
    void reload();
    const again = () => void reload();
    const t = setInterval(again, POLL_MS);
    window.addEventListener(ALERTS_CHANGED, again);
    return () => {
      clearInterval(t);
      window.removeEventListener(ALERTS_CHANGED, again);
    };
  }, [reload]);
  return { reply, error, reload };
}

/** The bell in the nav: a badge with the count and a dropdown of the worst few. Hidden when alerts cannot be read (a tenant token). */
export function AlertsBell() {
  const { reply, error } = useAlerts(false);
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const away = (e: MouseEvent) => ref.current && !ref.current.contains(e.target as Node) && setOpen(false);
    const esc = (e: KeyboardEvent) => e.key === 'Escape' && setOpen(false);
    const moved = () => setOpen(false);
    document.addEventListener('mousedown', away);
    document.addEventListener('keydown', esc);
    window.addEventListener('hashchange', moved);
    return () => {
      document.removeEventListener('mousedown', away);
      document.removeEventListener('keydown', esc);
      window.removeEventListener('hashchange', moved);
    };
  }, [open]);
  if (error && !reply) return null;
  const b = badge(reply);
  const top = (reply?.items ?? []).slice(0, 5);
  return (
    <div ref={ref} style={{ position: 'relative' }}>
      <button className="fx-icon-btn" aria-label={b.label} aria-expanded={open} data-testid="alerts-bell" onClick={() => setOpen((o) => !o)} style={{ position: 'relative' }}>
        <Bell size={16} />
        {b.count > 0 ? <span data-testid="alerts-badge" style={{ position: 'absolute', top: -4, right: -4, minWidth: 16, height: 16, borderRadius: 8, padding: '0 4px', fontSize: 11, lineHeight: '16px', textAlign: 'center', color: b.tone === 'bad' ? '#fff' : '#1d1d1f', background: b.tone === 'bad' ? 'var(--danger)' : 'var(--accent-amber)' }}>{b.count > 99 ? '99+' : b.count}</span> : null}
      </button>
      {open ? (
        <div role="dialog" aria-label="Alerts" style={{ position: 'absolute', right: 0, top: '120%', width: 360, maxWidth: '90vw', zIndex: 50, padding: 12, borderRadius: 10, border: '1px solid var(--border)', background: 'var(--bg-elevated)', color: 'var(--text-primary)', textAlign: 'left', boxShadow: '0 8px 24px #0004' }}>
          <b>{b.label}</b>
          {top.map((a) => (
            <div key={a.key} style={{ marginTop: 8 }}>
              <Pill tone={levelTone(a.level)}>{a.level === 'error' ? 'Error' : 'Warning'}</Pill> {a.summary}
              <div className="fx-muted">{a.next_step}</div>
            </div>
          ))}
          <div style={{ marginTop: 10 }}>
            <Button size="sm" onClick={() => { setOpen(false); navigate('/alerts'); }}>{b.count > top.length ? `See all ${b.count}` : 'Open alerts'}</Button>
          </div>
        </div>
      ) : null}
    </div>
  );
}

/** What needs attention now, each with the next step and a snooze. Mirrors `fluxctl alerts`. */
export function AlertsPage({ notify }: { notify: Notify }) {
  const [showSnoozed, setShowSnoozed] = useState(false);
  const [since, setSince] = useState<string>(WINDOWS[0]);
  const { reply, error, reload } = useAlerts(showSnoozed, since);
  const words = error ? errorWords(error) : null;

  const change = async (path: string, body: object, done: string, command: string) => {
    try {
      await api.post(path, body);
      notify(done, 'ok');
      window.dispatchEvent(new Event(ALERTS_CHANGED));
    } catch (e) {
      notify(changeFailed((e as Error).message, command), 'bad');
      await reload();
    }
  };
  const snooze = (a: Alert) => change('/v1/alerts/snooze', snoozeBody(a.key), `Snoozed for ${SNOOZE_HOURS} hours. It comes back if it is still wrong then.`, snoozeCommand(a.key));
  const unsnooze = (a: Alert) => change('/v1/alerts/unsnooze', { key: a.key }, 'Back on the list.', unsnoozeCommand(a.key));

  const items = reply?.items ?? [];
  return (
    <>
      <Hero eyebrow="Alerts" title="What needs attention," accent="right now." lede="Warnings and errors of the last day, backups and schedules that failed, Mac instances that are unhealthy and cluster health problems, each with what to do next. The same list as fluxctl alerts." />
      <Card title={reply ? badge(reply).label : 'Alerts'} action={<><select className="fx-input" aria-label="How far back" value={since} onChange={(e) => setSince(e.target.value)} style={{ width: 'auto' }}>{WINDOWS.map((w) => <option key={w} value={w}>Last {w}</option>)}</select> <CopyCmd notify={notify} cmd={alertsCommand(since)} /> <Button size="sm" variant={showSnoozed ? 'primary' : 'ghost'} aria-pressed={showSnoozed} onClick={() => setShowSnoozed((v) => !v)}>{showSnoozed ? 'Snoozed: shown' : 'Snoozed: hidden'}</Button></>}>
        {words && reply ? <p className="fx-muted" role="alert">{words.title}: {words.body} The list below is the last one read.</p> : null}
        {words && !reply ? (
          <EmptyState title={words.title} body={words.body} />
        ) : !reply ? (
          <Skeleton h={140} />
        ) : items.length === 0 ? (
          <EmptyState title="All clear" body={emptyWords(reply)} />
        ) : (
          <div className="fx-table-wrap"><table className="fx-table">
            <thead><tr><th /><th>What is wrong</th><th>What to do</th><th /></tr></thead>
            <tbody>
              {items.map((a) => (
                <tr key={a.key} data-testid="alert-row">
                  <td><Pill tone={levelTone(a.level)}>{a.level === 'error' ? 'Error' : 'Warning'}</Pill></td>
                  <td>
                    <b>{a.summary}</b>
                    <div className="fx-muted" title={a.last_seen}>{ago(a.last_seen)}{a.count > 1 ? `, ${a.count} times` : ''} <code className="fx-mono">{a.key}</code></div>
                  </td>
                  <td>
                    {a.next_step}
                    {a.explain ? <div className="fx-muted">More: <code className="fx-mono">fluxctl explain {a.explain}</code></div> : null}
                  </td>
                  <td>{isSnoozed(a) ? <Button size="sm" variant="ghost" onClick={() => void unsnooze(a)}>Bring back</Button> : <Button size="sm" variant="ghost" onClick={() => void snooze(a)}>Snooze 24h</Button>}</td>
                </tr>
              ))}
            </tbody>
          </table></div>
        )}
        {(reply?.notes ?? []).map((n) => <p key={n} className="fx-muted" role="status">{n}</p>)}
      </Card>
    </>
  );
}
