import { Copy, ExternalLink } from 'lucide-react';
import { Button, Card, Pill, type Vm } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { connectRoutes, forwardsOf, webUrl } from '../lib/connect';

/** The ways into a machine, as commands to copy, with what each needs. Nothing here opens a port: it explains what is open. */
export function ConnectTab({ vm, notify, goConsole }: { vm: Vm; notify: Notify; goConsole: () => void }) {
  const host = location.hostname || 'localhost';
  const routes = connectRoutes(vm, host);
  const fwd = forwardsOf(vm);
  const copy = (text: string) => navigator.clipboard.writeText(text).then(() => notify('Copied'), () => notify('Copy failed; select the command and copy it by hand', 'bad'));
  return (
    <div className="fx-grid">
      <Card title="SSH">
        <div className="cn-routes">
          {routes.map((r) => (
            <div key={r.key} className="cn-route">
              <div className="cn-head"><b>{r.title}</b><Pill tone={r.ready ? 'ok' : 'idle'}>{r.ready ? 'ready' : 'not yet'}</Pill></div>
              {r.command ? (
                <div className="cn-cmd">
                  <code className="fx-mono">{r.command}</code>
                  <Button size="sm" variant="ghost" aria-label={`Copy: ${r.command}`} onClick={() => void copy(r.command)}><Copy size={14} /></Button>
                </div>
              ) : null}
              <p className="fx-muted">{r.note}</p>
            </div>
          ))}
        </div>
      </Card>
      <Card title="Without SSH">
        <p className="fx-muted">The Console tab opens a shell over the guest agent (vsock), so it works with no network and no sshd in the machine.</p>
        <Button onClick={goConsole} disabled={vm.status !== 'running'}>Open the Console</Button>
      </Card>
      <Card title="Published ports">
        {fwd.length === 0 ? (
          <p className="fx-muted">No port of this machine is published on the host. Set forwards when you create a machine to reach a service from outside it.</p>
        ) : (
          <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>On the host</th><th>In the machine</th><th /></tr></thead><tbody>
            {fwd.map((f) => {
              const url = webUrl(f, host);
              return (
                <tr key={`${f.host_port}:${f.guest_port}`}>
                  <td className="fx-mono">{host}:{f.host_port}</td>
                  <td className="fx-mono">{f.guest_port}{f.guests ? <span className="fx-muted"> (also other guests)</span> : null}</td>
                  <td>{url ? <a className="fx-link" href={url} target="_blank" rel="noopener noreferrer"><ExternalLink size={13} /> Open</a> : null}</td>
                </tr>
              );
            })}
          </tbody></table></div>
        )}
        <p className="fx-muted">Reaching the machine from the internet is done through your fleet’s central (tenant sign-in, a port allow-list, tunnels that close when idle), not by opening ports on this Mac.</p>
      </Card>
    </div>
  );
}
