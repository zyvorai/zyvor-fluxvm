import { useCallback, useEffect, useRef, useState } from 'react';
import { Keyboard, Maximize, Plug, Unplug } from 'lucide-react';
import type RFB from '@novnc/novnc';
import { api } from './api';
import { Button, Pill } from './components';
import { wsUrl } from './ws';

type State = 'connecting' | 'open' | 'closed';

/**
 * A QEMU machine's graphical console over VNC. `ticketPath` is the API route that mints a single-use WebSocket ticket for the
 * machine (`/v1/vms/ID/ws-ticket` on a node, `/fleet/vms/NODE/ID/ws-ticket` on central, which relays through the node); the
 * server bridges the socket to the VM's VNC socket and noVNC (loaded on first use, so it stays out of the main bundle) speaks
 * RFB over it.
 */
export function VncScreen({ ticketPath, name, notify, footer }: {
  ticketPath: string;
  name: string;
  notify: (text: string, tone?: 'ok' | 'bad') => void;
  footer?: string;
}) {
  const host = useRef<HTMLDivElement>(null);
  const rfb = useRef<RFB | null>(null);
  const alive = useRef(true);
  const [state, setState] = useState<State>('connecting');
  const [err, setErr] = useState('');
  const [viewOnly, setViewOnly] = useState(false);
  const [fit, setFit] = useState(true);
  const [paste, setPaste] = useState('');

  const stop = useCallback(() => {
    const r = rfb.current;
    rfb.current = null;
    try { r?.disconnect(); } catch { /* already closed */ }
  }, []);

  const start = useCallback(async () => {
    stop();
    setErr('');
    setState('connecting');
    try {
      const [{ default: RFBClass }, ticket] = await Promise.all([import('@novnc/novnc'), api.post<{ path: string }>(ticketPath, { kind: 'vnc' })]);
      if (!alive.current || !host.current) return;
      host.current.replaceChildren();
      const r = new RFBClass(host.current, wsUrl(ticket.path), { shared: true });
      r.scaleViewport = true;
      r.background = '#0b0b0d';
      r.addEventListener('connect', () => alive.current && (setState('open'), r.focus()));
      r.addEventListener('disconnect', (e) => {
        if (!alive.current || rfb.current !== r) return;
        rfb.current = null;
        setState('closed');
        if (!(e.detail as { clean?: boolean }).clean) setErr('The connection ended unexpectedly. The machine may have stopped, or the ticket expired.');
      });
      r.addEventListener('securityfailure', (e) => alive.current && setErr(`The VNC server refused the connection: ${(e.detail as { reason?: string }).reason ?? 'security failure'}`));
      r.addEventListener('credentialsrequired', () => {
        // QEMU's VNC socket takes no password; a server that asks for one is not what this expects.
        setErr('The VNC server asked for a password, which this console does not send.');
        stop();
      });
      rfb.current = r;
    } catch (e) {
      if (!alive.current) return;
      setErr((e as Error).message);
      setState('closed');
    }
  }, [ticketPath, stop]);

  useEffect(() => {
    alive.current = true;
    void start();
    return () => {
      alive.current = false;
      stop();
    };
  }, [start, stop]);

  useEffect(() => { if (rfb.current) rfb.current.viewOnly = viewOnly; }, [viewOnly, state]);
  useEffect(() => { if (rfb.current) rfb.current.scaleViewport = fit; }, [fit, state]);

  return (
    <div className="vnc">
      <div className="vnc-bar">
        <Pill tone={state === 'open' ? 'ok' : state === 'connecting' ? 'info' : 'idle'}>{state}</Pill>
        {state === 'open' || state === 'connecting' ? <Button size="sm" onClick={() => { stop(); setState('closed'); }}><Unplug size={14} />Disconnect</Button> : <Button size="sm" variant="primary" onClick={() => void start()}><Plug size={14} />{err ? 'Reconnect' : 'Connect'}</Button>}
        <Button size="sm" disabled={state !== 'open' || viewOnly} onClick={() => rfb.current?.sendCtrlAltDel()}><Keyboard size={14} />Ctrl+Alt+Del</Button>
        <Button size="sm" disabled={state !== 'open'} onClick={() => void host.current?.requestFullscreen?.().catch(() => notify('The browser did not allow full screen', 'bad'))}><Maximize size={14} />Full screen</Button>
        <label className="vnc-opt"><input type="checkbox" checked={viewOnly} onChange={(e) => setViewOnly(e.target.checked)} /> view only</label>
        <label className="vnc-opt"><input type="checkbox" checked={fit} onChange={(e) => setFit(e.target.checked)} /> fit to window</label>
      </div>
      {err ? <div className="fx-error" role="alert">{err}</div> : null}
      <div className="vnc-screen" ref={host} aria-label={`The screen of ${name}`} />
      <form className="vnc-paste" onSubmit={(e) => { e.preventDefault(); if (paste && rfb.current && !viewOnly) { rfb.current.clipboardPasteFrom(paste); notify('Sent to the machine’s clipboard'); setPaste(''); } }}>
        <input className="fx-input" aria-label="Text to put on the machine's clipboard" placeholder="Text for the machine's clipboard…" value={paste} onChange={(e) => setPaste(e.target.value)} disabled={state !== 'open' || viewOnly} />
        <Button size="sm" type="submit" disabled={!paste || state !== 'open' || viewOnly}>Send</Button>
      </form>
      <p className="fx-muted">{footer ?? 'Clicks and keys go to the machine while it has focus. The console is a window onto the same screen as any other VNC client.'}</p>
    </div>
  );
}
