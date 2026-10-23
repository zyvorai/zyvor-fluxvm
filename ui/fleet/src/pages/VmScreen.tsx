import { useCallback, useEffect, useRef, useState } from 'react';
import { Button, api, apiBase, getToken } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { type InputAction, keyAction, mergeTypes, screenPoint, scrollNotches, typable } from '../lib/vmtools';
import type { FleetVm } from '../lib/fleet';

const enc = encodeURIComponent;

/** A VM's display, refreshed every 2 s (0.7 s while controlling), and, for a macOS guest, clicks, scrolling and keys sent to it. */
export function VmScreen({ vm, canControl, notify }: { vm: FleetVm; canControl: boolean; notify: Notify }) {
  const base = `/fleet/vms/${enc(vm.node)}/${enc(vm.id)}`;
  const imgRef = useRef<HTMLImageElement>(null);
  const [src, setSrc] = useState('');
  const [note, setNote] = useState('Connecting…');
  const [paused, setPaused] = useState(false);
  const [control, setControl] = useState(false);
  const [text, setText] = useState('');
  const live = useRef({ paused, control, key: `${vm.node}/${vm.id}`, seq: 0, url: '' });
  live.current.paused = paused;
  live.current.control = control;
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const queue = useRef<InputAction[]>([]);
  const flushTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const busy = useRef(false);
  const alive = useRef(true);

  const fetchScreen = useCallback(async () => {
    clearTimeout(timer.current);
    if (!alive.current || live.current.paused) return;
    const again = (ms: number) => { timer.current = setTimeout(() => void fetchScreen(), ms); };
    if (document.visibilityState !== 'visible') return again(4000);
    const seq = ++live.current.seq;
    try {
      const token = getToken();
      const r = await fetch(`${apiBase()}${base}/screen?max_width=${live.current.control ? 1600 : 960}`, { headers: token ? { Authorization: `Bearer ${token}` } : {}, cache: 'no-store' });
      if (!alive.current || seq !== live.current.seq) return;
      if (!r.ok) {
        const body = (await r.json().catch(() => ({}))) as { error?: string };
        setNote(r.status === 409 ? `No screen for this VM. ${body.error ?? ''}` : `The screen could not be loaded: ${body.error ?? `${r.status} ${r.statusText}`}`);
        return again(r.status === 409 ? 15000 : 6000);
      }
      const url = URL.createObjectURL(await r.blob());
      if (live.current.url) URL.revokeObjectURL(live.current.url);
      live.current.url = url;
      setSrc(url);
      const w = r.headers.get('x-fluxvm-screen-width');
      const h = r.headers.get('x-fluxvm-screen-height');
      setNote(live.current.control
        ? 'Controlling the VM: clicks, scrolling and keys go to it. Press Stop control when you are done.'
        : `${r.headers.get('x-fluxvm-screen-blank') === 'true' ? 'Nothing is drawn yet (the guest is still starting). ' : ''}${w && h ? `${w} × ${h}. ` : ''}Updates every 2 seconds while this panel is open.`);
      again(live.current.control ? 700 : 2000);
    } catch {
      if (alive.current) { setNote('Central is not reachable; retrying.'); again(6000); }
    }
  }, [base]);

  useEffect(() => {
    alive.current = true;
    void fetchScreen();
    return () => {
      alive.current = false;
      clearTimeout(timer.current);
      clearTimeout(flushTimer.current);
      if (live.current.url) URL.revokeObjectURL(live.current.url);
    };
  }, [fetchScreen]);

  const flush = useCallback(async () => {
    if (busy.current || !queue.current.length) return;
    busy.current = true;
    const actions = mergeTypes(queue.current.splice(0, 200));
    try { await api.post(`${base}/input`, { actions }); } catch (e) { notify((e as Error).message, 'bad'); }
    busy.current = false;
    if (!alive.current) return;
    clearTimeout(timer.current);
    timer.current = setTimeout(() => void fetchScreen(), 250);
    if (queue.current.length) void flush();
  }, [base, fetchScreen, notify]);

  const send = useCallback((actions: InputAction[]) => {
    queue.current.push(...actions);
    clearTimeout(flushTimer.current);
    flushTimer.current = setTimeout(() => void flush(), 80);
  }, [flush]);

  const point = (e: { clientX: number; clientY: number }) => {
    const img = imgRef.current;
    if (!img) return null;
    const r = img.getBoundingClientRect();
    return screenPoint(r, { w: img.naturalWidth, h: img.naturalHeight }, e.clientX, e.clientY);
  };
  // A double click would reach the guest as a click and then a double click; a single click waits to see whether a second follows.
  const pending = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const wheelAt = useRef(0);

  const toggleControl = (on: boolean) => {
    setControl(on);
    live.current.control = on;
    setPaused(false);
    live.current.paused = false;
    void fetchScreen();
    if (on) setTimeout(() => imgRef.current?.focus(), 0);
  };
  const keyBtn = (label: string, key: string, modifiers: string[] = []) => <Button key={label} size="sm" onClick={() => send([{ action: 'key', key, modifiers }])}>{label}</Button>;

  return (
    <div>
      <div className="fx-actions" style={{ marginBottom: 10 }}>
        <Button size="sm" onClick={() => { const p = !paused; setPaused(p); live.current.paused = p; if (!p) void fetchScreen(); }}>{paused ? 'Resume' : 'Pause'}</Button>
        {canControl ? <Button size="sm" variant={control ? 'primary' : undefined} aria-pressed={control} onClick={() => toggleControl(!control)}>{control ? 'Stop control' : 'Control'}</Button> : null}
        {src ? <a className="fx-link" href={src} target="_blank" rel="noopener noreferrer">Open full size</a> : null}
      </div>
      <div className={`fx-screen${control ? ' is-control' : ''}`}>
        {src ? (
          <img
            ref={imgRef}
            src={src}
            alt={`The screen of ${vm.name}`}
            tabIndex={control ? 0 : undefined}
            draggable={false}
            onClick={(e) => {
              if (!control || e.detail > 1) return;
              const p = point(e);
              if (!p) return;
              clearTimeout(pending.current);
              pending.current = setTimeout(() => { pending.current = undefined; send([{ action: 'click', ...p }]); }, 250);
            }}
            onDoubleClick={(e) => {
              if (!control) return;
              clearTimeout(pending.current);
              const p = point(e);
              if (p) send([{ action: 'double_click', ...p }]);
            }}
            onContextMenu={(e) => {
              if (!control) return;
              e.preventDefault();
              const p = point(e);
              if (p) send([{ action: 'right_click', ...p }]);
            }}
            onWheel={(e) => {
              if (!control || Date.now() - wheelAt.current < 60) return;
              wheelAt.current = Date.now();
              send([{ action: 'scroll', dx: scrollNotches(e.deltaX), dy: scrollNotches(e.deltaY), ...point(e) }]);
            }}
            onKeyDown={(e) => {
              if (!control) return;
              // Every key belongs to the VM while controlling: Escape must not close the sheet, nor ⌘K open the palette.
              e.stopPropagation();
              e.nativeEvent.stopImmediatePropagation();
              const a = keyAction(e);
              if (a) { e.preventDefault(); send([a]); }
            }}
          />
        ) : <div className="fx-screen-empty fx-muted">{note}</div>}
      </div>
      <p className="fx-muted" role="status">{note}</p>
      {control ? (
        <form className="fx-actions" onSubmit={(e) => { e.preventDefault(); if (text && typable(text)) { send([{ action: 'type', text }]); setText(''); } }}>
          <input className="fx-input" aria-label="Text to type into the VM" placeholder="Type or paste text…" value={text} onChange={(e) => setText(e.target.value)} />
          <Button size="sm" type="submit" disabled={!text || !typable(text)}>Type</Button>
          {keyBtn('Return', 'return')}{keyBtn('Esc', 'escape')}{keyBtn('Tab', 'tab')}{keyBtn('⌘ Space', 'space', ['command'])}
          {text && !typable(text) ? <span className="fx-error" role="alert">Only printable ASCII, newline and tab can be typed.</span> : null}
        </form>
      ) : null}
    </div>
  );
}
