import { useState } from 'react';
import { Card, Segmented, api } from '@fluxvm/ui-core';
import { TerminalView } from './TerminalView';

type Kind = 'console' | 'serial';

/** Guest shell (through the guest agent) or the serial port, in the browser. */
export function ConsoleTab({ id, running }: { id: string; running: boolean }) {
  const [kind, setKind] = useState<Kind>('console');
  const connect = async (cols: number, rows: number) => {
    const t = await api.post<{ ticket: string }>(`/v1/vms/${id}/ws-ticket`, { kind });
    return `/console/ws/${t.ticket}?cols=${cols}&rows=${rows}`;
  };
  return (
    <Card title="Console" action={<Segmented label="Console type" value={kind} onChange={setKind} options={[{ value: 'console', label: 'Shell' }, { value: 'serial', label: 'Serial' }]} />}>
      {!running ? <p className="fx-muted">Start the machine to open a console.</p> : null}
      {/* key: switching type remounts, so each connection gets a fresh terminal and ticket */}
      <TerminalView key={kind} connect={connect} resize={kind === 'console' ? 'flat' : 'none'} hint={kind === 'console' ? 'Interactive shell through the guest agent.' : 'Raw serial port. Works before the guest agent is up; on non-QEMU backends it is a read-only log.'} />
    </Card>
  );
}
