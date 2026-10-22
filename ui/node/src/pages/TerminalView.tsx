import { useEffect, useRef, useState } from 'react';
import { Plug, Unplug } from 'lucide-react';
import { Button, Pill } from '@fluxvm/ui-core';
import { type ResizeFormat, resizeMessage, termTheme, wsUrl } from '../lib/ws';

type State = 'idle' | 'connecting' | 'open' | 'closed';

/**
 * A browser terminal over a daemon WebSocket. `connect` mints a ticket and returns the same-origin path to open
 * (`/console/ws/<ticket>?cols=..&rows=..`). xterm.js is loaded on first use so it stays out of the main bundle.
 * Binary frames carry data both ways. `resize` also sends text frames on resize: 'flat' is `{cols,rows}` (the VM
 * guest shell), 'nested' is `{resize:{cols,rows}}` (agent-session terminals); the serial port takes none.
 */
export function TerminalView({ connect, resize = 'none', readOnly = false, hint }: { connect: (cols: number, rows: number) => Promise<string>; resize?: ResizeFormat; readOnly?: boolean; hint?: string }) {
  const host = useRef<HTMLDivElement>(null);
  const ref = useRef<{ dispose: () => void; start: () => Promise<void>; stop: () => void } | null>(null);
  const [state, setState] = useState<State>('idle');
  const [err, setErr] = useState('');

  useEffect(() => {
    let disposed = false;
    let ws: WebSocket | null = null;
    let term: import('@xterm/xterm').Terminal | null = null;
    let fit: import('@xterm/addon-fit').FitAddon | null = null;
    let ro: ResizeObserver | null = null;
    const enc = new TextEncoder();

    const ensureTerm = async () => {
      if (term) return term;
      const [{ Terminal }, { FitAddon }] = await Promise.all([import('@xterm/xterm'), import('@xterm/addon-fit'), import('@xterm/xterm/css/xterm.css')]);
      if (disposed || !host.current) throw new Error('closed');
      term = new Terminal({ cursorBlink: true, fontFamily: 'ui-monospace, "SF Mono", Menlo, Consolas, monospace', fontSize: 13, theme: termTheme(), scrollback: 5000, disableStdin: readOnly });
      fit = new FitAddon();
      term.loadAddon(fit);
      term.open(host.current);
      fit.fit();
      term.onData((d) => ws?.readyState === WebSocket.OPEN && ws.send(enc.encode(d)));
      term.onResize(({ cols, rows }) => resize !== 'none' && ws?.readyState === WebSocket.OPEN && ws.send(resizeMessage(cols, rows, resize)));
      ro = new ResizeObserver(() => fit?.fit());
      ro.observe(host.current);
      return term;
    };

    ref.current = {
      start: async () => {
        setErr('');
        setState('connecting');
        try {
          const t = await ensureTerm();
          t.reset();
          fit?.fit();
          const path = await connect(t.cols, t.rows);
          ws = new WebSocket(wsUrl(path));
          ws.binaryType = 'arraybuffer';
          ws.onopen = () => (setState('open'), t.focus());
          ws.onmessage = (e) => t.write(typeof e.data === 'string' ? e.data : new Uint8Array(e.data as ArrayBuffer));
          ws.onclose = () => setState((s) => (s === 'connecting' ? (setErr('Could not open the connection. The ticket may have expired or the machine is not reachable.'), 'closed') : 'closed'));
          ws.onerror = () => setErr('The connection failed.');
        } catch (e) {
          if ((e as Error).message !== 'closed') setErr((e as Error).message);
          setState('closed');
        }
      },
      stop: () => ws?.close(),
      dispose: () => {
        disposed = true;
        ws?.close();
        ro?.disconnect();
        term?.dispose();
      },
    };
    return () => ref.current?.dispose();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <div className="fx-term">
      <div className="fx-term-bar">
        <Pill tone={state === 'open' ? 'ok' : state === 'connecting' ? 'info' : 'idle'}>{state === 'idle' ? 'not connected' : state}</Pill>
        {hint ? <span className="fx-muted">{hint}</span> : null}
        <span style={{ marginLeft: 'auto' }}>
          {state === 'open' || state === 'connecting' ? <Button size="sm" onClick={() => ref.current?.stop()}><Unplug size={14} />Disconnect</Button> : <Button size="sm" variant="primary" onClick={() => void ref.current?.start()}><Plug size={14} />{state === 'closed' ? 'Reconnect' : 'Connect'}</Button>}
        </span>
      </div>
      {err ? <div className="fx-error" role="alert">{err}</div> : null}
      <div className="fx-term-body" ref={host} />
    </div>
  );
}
