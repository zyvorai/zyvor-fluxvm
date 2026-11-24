// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { useCallback, useEffect, useRef, useState } from 'react';
import { Button, Card, EmptyState, Hero, Pill, Skeleton, api } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { CopyCmd } from './Images';
import { EMPTY_FILTERS, LEVEL_CHOICES, SINCE_CHOICES, SOURCES, emptyWords, errorWords, eventsCommand, eventsPath, feedNotes, levelTone, mergeNew, newestTs, sourceLabel, wantsK8s, type EventFilters, type FeedEvent, type FeedReply } from '../lib/events';

const POLL_MS = 3000;

/** Everything that happened, in one list: VMs, Macs, Kubernetes, containers, backups and the audit log. Mirrors `fluxctl events`. */
export function EventsPage({ notify }: { notify: Notify }) {
  const [filters, setFilters] = useState<EventFilters>(EMPTY_FILTERS);
  const [items, setItems] = useState<FeedEvent[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [notes, setNotes] = useState<string[]>([]);
  const [loading, setLoading] = useState(true);
  const [live, setLive] = useState(false);
  const newest = useRef<string | undefined>(undefined);

  const load = useCallback(async (f: EventFilters) => {
    setLoading(true);
    try {
      const r = await api.get<FeedReply>(eventsPath(f));
      // The list reads newest first; the API replies oldest first.
      const list = [...r.items].reverse();
      setItems(list);
      newest.current = newestTs(list);
      setNotes(feedNotes(r));
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    const t = setTimeout(() => void load(filters), 250);
    return () => clearTimeout(t);
  }, [filters, load]);

  // Live follow: ask only for what is newer than the newest line shown.
  useEffect(() => {
    if (!live) return;
    const t = setInterval(() => {
      const since = newest.current;
      if (!since) return;
      api.get<FeedReply>(eventsPath(filters, 200, since))
        .then((r) => {
          if (r.items.length) {
            setItems((cur) => {
              const next = mergeNew(cur, r.items);
              newest.current = newestTs(next);
              return next;
            });
          }
          setNotes(feedNotes(r));
          setError(null);
        })
        .catch((e) => {
          setLive(false);
          setError((e as Error).message);
          notify('Live feed stopped. Turn Live on again to resume.', 'bad');
        });
    }, POLL_MS);
    return () => clearInterval(t);
  }, [live, filters, notify]);

  const toggle = (s: string) => setFilters((f) => ({ ...f, sources: f.sources.includes(s) ? f.sources.filter((x) => x !== s) : [...f.sources, s] }));
  const words = error ? errorWords(error) : null;
  return (
    <>
      <Hero eyebrow="Events" title="Everything that happened," accent="in one list." lede="Machine starts and stops, Mac and Kubernetes changes, containers, backup and schedule runs and the audit log, merged by time. The same list as fluxctl events." />
      <Card title="Show" action={<><CopyCmd notify={notify} cmd={eventsCommand(filters, live)} /> <Button size="sm" variant={live ? 'primary' : 'ghost'} onClick={() => setLive((v) => !v)}>{live ? 'Live: on' : 'Live: off'}</Button></>}>
        <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap', alignItems: 'center' }}>
          {SOURCES.map((s) => {
            const on = filters.sources.length === 0 || filters.sources.includes(s.value);
            return <Button key={s.value} size="sm" variant={filters.sources.includes(s.value) ? 'primary' : 'ghost'} aria-pressed={on} title={s.about} onClick={() => toggle(s.value)}>{s.label}</Button>;
          })}
          <select className="fx-input" style={{ width: 'auto' }} aria-label="When" value={filters.since} onChange={(e) => setFilters((f) => ({ ...f, since: e.target.value }))}>
            {SINCE_CHOICES.map((s) => <option key={s.value} value={s.value}>{s.label}</option>)}
          </select>
          <select className="fx-input" style={{ width: 'auto' }} aria-label="Level" value={filters.level} onChange={(e) => setFilters((f) => ({ ...f, level: e.target.value }))}>
            {LEVEL_CHOICES.map((s) => <option key={s.value} value={s.value}>{s.label}</option>)}
          </select>
          <input className="fx-input" style={{ width: 170 }} aria-label="Event name" placeholder="Event, e.g. mac.run" value={filters.event} onChange={(e) => setFilters((f) => ({ ...f, event: e.target.value }))} />
          {wantsK8s(filters) ? <input className="fx-input" style={{ width: 150 }} aria-label="Cluster" placeholder="Cluster (all)" value={filters.cluster ?? ''} onChange={(e) => setFilters((f) => ({ ...f, cluster: e.target.value.trim().toLowerCase() }))} /> : null}
          <Button size="sm" variant="ghost" onClick={() => setFilters(EMPTY_FILTERS)}>Clear</Button>
        </div>
        <p className="fx-muted">No source selected shows all of them. Cluster events are read live from running clusters (not while following).</p>
        {notes.map((n) => <p key={n} className="fx-muted" role="status">{n}</p>)}
      </Card>
      <Card title={`${items.length} ${items.length === 1 ? 'event' : 'events'}`}>
        {words ? (
          <EmptyState title={words.title} body={words.body} />
        ) : loading && items.length === 0 ? (
          <Skeleton h={140} />
        ) : items.length === 0 ? (
          <EmptyState title="Nothing here" body={emptyWords(filters)} />
        ) : (
          <div className="fx-table-wrap"><table className="fx-table">
            <thead><tr><th>When</th><th>From</th><th>What happened</th><th /></tr></thead>
            <tbody>
              {items.map((e, i) => (
                <tr key={`${e.ts}-${e.event}-${i}`}>
                  <td title={e.ts}>{new Date(e.ts).toLocaleString()}</td>
                  <td>{sourceLabel(e.source)}</td>
                  <td><b>{e.summary}</b><div className="fx-muted"><code className="fx-mono">{e.event}</code></div></td>
                  <td>{e.level === 'info' ? null : <Pill tone={levelTone(e.level)}>{e.level === 'error' ? 'Error' : 'Warning'}</Pill>}</td>
                </tr>
              ))}
            </tbody>
          </table></div>
        )}
      </Card>
    </>
  );
}
