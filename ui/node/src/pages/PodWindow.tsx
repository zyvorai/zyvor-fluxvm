import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Check, Copy, Download, Pause, Play, Trash2, WrapText, X } from 'lucide-react';
import { api, usePoll } from '@fluxvm/ui-core';
import { type KubePod, ageOf } from '../lib/kube';
import { LEVEL_ORDER, type LogLine, levelCounts, linesOf, matches, renderLine, shortTime } from '../lib/logs';
import { TerminalView } from './TerminalView';

const enc = encodeURIComponent;
const TAILS = [200, 500, 1000, 2000, 5000];

export type PodTab = 'logs' | 'shell';

/** A pod's logs and a shell in it, in a window dressed like Terminal.app. */
export function PodWindow({ cluster, pod, tab: initial, onClose }: { cluster: string; pod: KubePod; tab: PodTab; onClose: () => void }) {
  const [tab, setTab] = useState<PodTab>(initial);
  const containers = pod.containers?.length ? pod.containers : [];
  const [container, setContainer] = useState(containers[0] ?? '');
  useEffect(() => {
    const f = (e: KeyboardEvent) => e.key === 'Escape' && onClose();
    window.addEventListener('keydown', f);
    return () => window.removeEventListener('keydown', f);
  }, [onClose]);
  const base = `/v1/kube/${enc(cluster)}/pods/${enc(pod.namespace)}/${enc(pod.name)}`;
  return (
    <div className="kx-wrap" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <section className="kx-window" role="dialog" aria-modal="true" aria-label={`${pod.name} ${tab}`}>
        <header className="kx-titlebar">
          <span className="kx-dots">
            <button className="kx-dot kx-dot-close" aria-label="Close" onClick={onClose}><X size={8} strokeWidth={3} /></button>
            <span className="kx-dot kx-dot-min" aria-hidden />
            <span className="kx-dot kx-dot-max" aria-hidden />
          </span>
          <span className="kx-title">
            <b>{pod.name}</b>
            <span className="kx-sub"> — {container || 'pod'} — {cluster}/{pod.namespace} — {pod.status}</span>
          </span>
          <span className="kx-tabs" role="tablist">
            <button role="tab" aria-selected={tab === 'logs'} className={tab === 'logs' ? 'is-on' : ''} onClick={() => setTab('logs')}>Logs</button>
            <button role="tab" aria-selected={tab === 'shell'} className={tab === 'shell' ? 'is-on' : ''} onClick={() => setTab('shell')}>Shell</button>
          </span>
        </header>
        {containers.length > 1 ? (
          <div className="kx-sub-bar">
            <label>container
              <select value={container} onChange={(e) => setContainer(e.target.value)} aria-label="Container">
                {containers.map((c) => <option key={c}>{c}</option>)}
              </select>
            </label>
          </div>
        ) : null}
        {tab === 'logs' ? <LogsPane base={base} container={container} name={pod.name} up={`pod up ${ageOf(pod.created, Date.now())}`} /> : <ShellPane base={base} container={container} />}
      </section>
    </div>
  );
}

/** A log with a filter, a tail length, follow, wrap, clear, copy and download. `base` + `/` + `logPath` is the route that returns the text. */
export function LogsPane({ base, container, name, up, logPath = 'log', canPrevious = true }: { base: string; container: string; name: string; up: string; logPath?: string; canPrevious?: boolean }) {
  const [tail, setTail] = useState(500);
  const [follow, setFollow] = useState(true);
  const [previous, setPrevious] = useState(false);
  const [wrap, setWrap] = useState(true);
  const [filter, setFilter] = useState('');
  const [cleared, setCleared] = useState(0);
  const [copied, setCopied] = useState(false);
  const body = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);

  const q = `?tail=${tail}${container ? `&container=${enc(container)}` : ''}${previous ? '&previous=true' : ''}`;
  const poll = usePoll(async () => await api.get<string>(`${base}/${logPath}${q}`, { text: true }), follow ? 2000 : 3_600_000, [q]);
  const all: LogLine[] = useMemo(() => linesOf(poll.data ?? '').map((l, i) => renderLine(i + 1, l)), [poll.data]);
  const shown = useMemo(() => all.filter((l) => l.n > cleared && matches(l.raw, filter)), [all, cleared, filter]);
  const counts = useMemo(() => levelCounts(shown), [shown]);

  useEffect(() => {
    const el = body.current;
    if (el && pinned.current) el.scrollTop = el.scrollHeight;
  }, [shown]);
  const onScroll = () => {
    const el = body.current;
    if (el) pinned.current = el.scrollHeight - el.scrollTop - el.clientHeight < 32;
  };
  const text = useCallback(() => shown.map((l) => l.raw).join('\n') + '\n', [shown]);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text());
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* the browser refused; the text can still be selected by hand */
    }
  };
  const download = () => {
    const a = document.createElement('a');
    a.href = URL.createObjectURL(new Blob([text()], { type: 'text/plain' }));
    a.download = `${name}${container ? '-' + container : ''}.log`;
    a.click();
    setTimeout(() => URL.revokeObjectURL(a.href), 2000);
  };
  const err = poll.error && !poll.data ? poll.error.message : '';

  return (
    <>
      <div className="kx-bar">
        <input aria-label="Filter log lines" placeholder="Filter  (words, -exclude)" value={filter} onChange={(e) => setFilter(e.target.value)} />
        <select aria-label="Lines" value={tail} onChange={(e) => setTail(Number(e.target.value))}>{TAILS.map((t) => <option key={t} value={t}>last {t}</option>)}</select>
        {canPrevious ? <button className={previous ? 'is-on' : ''} onClick={() => setPrevious(!previous)} title="The previous container instance (after a restart)">previous</button> : null}
        <button className={wrap ? 'is-on' : ''} onClick={() => setWrap(!wrap)} aria-pressed={wrap} title="Wrap long lines"><WrapText size={13} /></button>
        <button className={follow ? 'is-on' : ''} onClick={() => setFollow(!follow)} aria-pressed={follow} title="Refresh every 2 seconds">{follow ? <Pause size={13} /> : <Play size={13} />} {follow ? 'following' : 'paused'}</button>
        <button onClick={() => setCleared(all.length ? all[all.length - 1].n : 0)} title="Clear the screen (the log stays in the pod)" aria-label="Clear"><Trash2 size={13} /></button>
        <button onClick={copy} title="Copy what is shown" aria-label="Copy">{copied ? <Check size={13} /> : <Copy size={13} />}</button>
        <button onClick={download} title="Download what is shown" aria-label="Download"><Download size={13} /></button>
      </div>
      <div ref={body} className={`kx-log${wrap ? ' is-wrap' : ''}`} onScroll={onScroll} role="log" aria-live="off" tabIndex={0}>
        {err ? <p className="kx-note kx-err">{err}</p> : null}
        {!err && poll.loading && !poll.data ? <p className="kx-note">loading…</p> : null}
        {!err && !poll.loading && shown.length === 0 ? <p className="kx-note">{all.length ? 'No line matches the filter.' : 'No log output yet.'}</p> : null}
        {shown.map((l) => (
          <div key={l.n} className={`kx-line${l.level ? ' lv-' + l.level : ''}`}>
            <span className="kx-n">{l.n}</span>
            {l.ts ? <span className="kx-ts" title={l.ts}>{shortTime(l.ts)}</span> : null}
            {l.level ? <span className={`kx-lv lv-${l.level}`}>{l.level}</span> : null}
            <span className="kx-msg">{l.segs.map((s, i) => (s.cls ? <span key={i} className={'kx-' + s.cls.split(' ').join(' kx-')}>{s.text}</span> : s.text))}</span>
          </div>
        ))}
      </div>
      <footer className="kx-foot">
        <span>{shown.length} line{shown.length === 1 ? '' : 's'}{all.length !== shown.length ? ` of ${all.length}` : ''}</span>
        {LEVEL_ORDER.filter((lv) => counts[lv]).map((lv) => <span key={lv} className={`kx-lv lv-${lv}`}>{counts[lv]} {lv}</span>)}
        <span className="kx-grow" />
        <span>{up}</span>
      </footer>
    </>
  );
}

function ShellPane({ base, container }: { base: string; container: string }) {
  const connect = useCallback(
    async (cols: number, rows: number) => {
      const r = await api.post<{ path: string }>(`${base}/exec-ticket`, container ? { container } : {});
      return `${r.path}?cols=${cols}&rows=${rows}`;
    },
    [base, container],
  );
  return (
    <div className="kx-shell">
      <TerminalView key={`${base}/${container}`} connect={connect} resize="flat" hint="A shell inside the pod (admin only). Distroless images have none." />
    </div>
  );
}
