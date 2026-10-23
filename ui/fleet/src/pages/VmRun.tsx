import { useRef, useState } from 'react';
import { Button, api } from '@fluxvm/ui-core';
import type { FleetVm } from '../lib/fleet';
import { type ExecResult, exitOf } from '../lib/vmtools';

const enc = encodeURIComponent;
interface Entry { cmd: string; pending: boolean; result?: ExecResult; error?: string }

/** Runs a command in the guest through its agent (60 s limit) and keeps the last twenty results. */
export function VmRun({ vm }: { vm: FleetVm }) {
  const [draft, setDraft] = useState('');
  const [log, setLog] = useState<Entry[]>([]);
  const end = useRef<HTMLDivElement>(null);
  const run = async () => {
    const cmd = draft.trim();
    if (!cmd) return;
    setDraft('');
    setLog((l) => [...l.slice(-19), { cmd, pending: true }]);
    let next: Partial<Entry>;
    try { next = { result: await api.post<ExecResult>(`/fleet/vms/${enc(vm.node)}/${enc(vm.id)}/exec`, { command: cmd, timeout_seconds: 60 }) }; } catch (e) { next = { error: (e as Error).message }; }
    setLog((l) => l.map((x) => (x.cmd === cmd && x.pending ? { ...x, ...next, pending: false } : x)));
    setTimeout(() => end.current?.scrollIntoView({ block: 'nearest' }), 0);
  };
  return (
    <div>
      <form className="fx-actions" onSubmit={(e) => { e.preventDefault(); void run(); }}>
        <input className="fx-input fx-mono" aria-label="Command" placeholder="uname -a" autoComplete="off" spellCheck={false} value={draft} onChange={(e) => setDraft(e.target.value)} />
        <Button variant="primary" type="submit" disabled={!draft.trim()}>Run</Button>
      </form>
      <p className="fx-muted">Runs through the guest agent as the agent&rsquo;s user, with a 60-second limit.</p>
      <div className="fx-term" aria-live="polite">
        {log.length === 0 ? <span className="fx-muted">Nothing run yet.</span> : log.map((x, i) => (
          <div key={i} className="fx-term-entry">
            <div className="fx-term-cmd">$ {x.cmd}</div>
            {x.pending ? <div className="fx-muted">running…</div> : x.error ? <div className="fx-code-err">{x.error}</div> : (
              <>
                {x.result?.stdout ? <pre>{x.result.stdout}</pre> : null}
                {x.result?.stderr ? <pre className="fx-code-err">{x.result.stderr}</pre> : null}
                <div className="fx-muted">exit {exitOf(x.result ?? {})}</div>
              </>
            )}
          </div>
        ))}
        <div ref={end} />
      </div>
    </div>
  );
}
