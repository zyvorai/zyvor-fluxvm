import { useEffect, useMemo, useState } from 'react';
import { Button, Card, Hero, Pill, Reveal, Skeleton, Stat, ago, api, navigate, statusTone, streamEvents, usePoll, type Density, type Vm } from '@fluxvm/ui-core';

interface Ev {
  at: number;
  name: string;
  who: string;
  detail: string;
}

const pick = (o: unknown, ...keys: string[]): string => {
  if (o && typeof o === 'object') for (const k of keys) {
    const v = (o as Record<string, unknown>)[k];
    if (typeof v === 'string' && v) return v;
  }
  return '';
};

export function Overview({ vms, setVms }: { vms: Vm[]; setVms: (v: Vm[]) => void }) {
  const list = usePoll(() => api.get<{ items: Vm[] }>('/v1/vms'), 5000);
  const density = usePoll(() => api.get<Density>('/v1/sandboxes/density').catch(() => null), 10000);
  const [events, setEvents] = useState<Ev[]>([]);
  const [live, setLive] = useState(false);

  useEffect(() => {
    if (list.data) setVms(list.data.items ?? []);
  }, [list.data, setVms]);

  useEffect(() => {
    const ac = new AbortController();
    streamEvents(
      '/v1/events/stream',
      (m) => {
        setLive(true);
        const d = m.data;
        setEvents((e) => [{ at: Date.now(), name: pick(d, 'type', 'kind', 'event') || m.event, who: pick(d, 'vm_name', 'name', 'vm_id', 'id'), detail: pick(d, 'message', 'status', 'detail') }, ...e].slice(0, 60));
        list.refresh();
      },
      ac.signal,
    ).finally(() => setLive(false));
    return () => ac.abort();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const c = useMemo(() => {
    const n = (s: string) => vms.filter((v) => v.status === s).length;
    return { total: vms.length, running: n('running'), stopped: n('stopped'), paused: n('paused'), failed: n('failed') };
  }, [vms]);
  const attention = vms.filter((v) => v.status === 'failed' || v.error);
  const pressure = density.data?.host_pressure_level;

  return (
    <>
      <Hero eyebrow="FluxVM Studio" title="Your machines," accent="in flow." lede="Create, observe and control every VM, sandbox and agent on this node. Press ⌘K for anything.">
        <Button variant="primary" size="lg" onClick={() => navigate('/machines')}>View machines</Button>
      </Hero>

      <div className="fx-grid cols-4">
        {list.loading && !list.data ? [0, 1, 2, 3].map((i) => <Skeleton key={i} h={104} />) : (
          <>
            <Reveal><Stat label="Machines" value={c.total} tone="info" /></Reveal>
            <Reveal delay={60}><Stat label="Running" value={c.running} tone="ok" /></Reveal>
            <Reveal delay={120}><Stat label="Stopped / paused" value={c.stopped + c.paused} tone="idle" /></Reveal>
            <Reveal delay={180}><Stat label="Needs attention" value={c.failed} tone={c.failed ? 'bad' : 'idle'} hint={pressure ? `Host memory pressure: ${pressure}` : undefined} /></Reveal>
          </>
        )}
      </div>

      <div className="fx-grid cols-2 fx-section">
        <Reveal>
          <Card title="Live activity" action={<span className={`fx-live ${live ? '' : 'is-off'}`}><i />{live ? 'Live' : 'Connecting…'}</span>}>
            {events.length === 0 ? <p className="fx-muted">Waiting for events. Start, stop or create a machine and it shows up here instantly.</p> : (
              <ul className="fx-feed">
                {events.map((e, i) => (
                  <li key={e.at + '-' + i}>
                    <Pill tone="info" dot={false} raw>{e.name}</Pill>
                    <span>{e.who} <span className="fx-muted">{e.detail}</span></span>
                    <time>{ago(e.at)}</time>
                  </li>
                ))}
              </ul>
            )}
          </Card>
        </Reveal>
        <Reveal delay={80}>
          <Card title="Needs attention">
            {attention.length === 0 ? <p className="fx-muted">Nothing is failing. Everything on this node looks healthy.</p> : (
              <ul className="fx-feed">
                {attention.map((v) => (
                  <li key={v.id} onClick={() => navigate('/machines?q=' + encodeURIComponent(v.name))} style={{ cursor: 'pointer' }}>
                    <Pill tone={statusTone(v.status)}>{v.status}</Pill>
                    <span>{v.name}<div className="vm-err">{v.error}</div></span>
                    <span className="fx-chev">Open</span>
                  </li>
                ))}
              </ul>
            )}
          </Card>
        </Reveal>
      </div>
    </>
  );
}
