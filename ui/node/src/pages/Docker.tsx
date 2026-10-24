import { useMemo, useState } from 'react';
import { ScrollText } from 'lucide-react';
import { ContainerWindow } from './ContainerWindow';
import { Button, Card, EmptyState, Hero, Pill, Skeleton, Tabs, api, usePoll } from '@fluxvm/ui-core';
import { type DockerContainer, type DockerView, ageSince, containerTone, filterContainers, fmtSize, portsText, summarize } from '../lib/docker';

/** The Docker engine `fluxctl docker` runs in its own VM: containers and images, and a container's log. Read-only. */
export function DockerPage() {
  const view = usePoll(async () => await api.get<DockerView>('/v1/docker'), 5000);
  const d = view.data;
  const [tab, setTab] = useState<'containers' | 'images'>('containers');
  const [group, setGroup] = useState<'all' | 'running' | 'stopped'>('all');
  const [q, setQ] = useState('');
  const [open, setOpen] = useState<DockerContainer | null>(null);
  const now = Date.now();
  const shown = useMemo(() => filterContainers(d?.containers ?? [], q, group), [d, q, group]);
  const sum = useMemo(() => summarize(d?.containers ?? []), [d]);
  const forbidden = view.error?.message && /admin/i.test(view.error.message);

  return (
    <>
      <Hero eyebrow="Docker" title="The engine on this host," accent="containers included." lede="The Docker engine fluxctl docker runs in its own VM. Read-only: this page shows what is running; use the docker CLI to change it." />
      {view.loading && !d ? (
        <Skeleton h={160} />
      ) : forbidden ? (
        <Card><EmptyState title="Admin only" body="The containers belong to the whole host, so only an administrator can see them." /></Card>
      ) : view.error && !d ? (
        <Card><EmptyState title="The daemon could not read the engine" body={view.error.message} /></Card>
      ) : d && !d.engine.reachable ? (
        <Card><EmptyState title={d.engine.vm ? `The engine is ${d.engine.vm.status}` : 'No Docker engine'} body={d.engine.reason ?? 'The engine did not answer.'} /></Card>
      ) : d ? (
        <>
          <Card title="Engine">
            <p className="fx-muted">
              <Pill tone="ok">reachable</Pill>{' '}
              Docker <span className="fx-mono">{d.engine.info?.version ?? '?'}</span> (API {d.engine.info?.api ?? '?'}) on {d.engine.info?.os ?? 'linux'}/{d.engine.info?.arch ?? '?'}, kernel <span className="fx-mono">{d.engine.info?.kernel ?? '?'}</span>
              {d.engine.vm ? <> · machine <b>{d.engine.vm.name}</b> ({d.engine.vm.status})</> : null}
              {' '}· socket <span className="fx-mono">{d.engine.socket}</span>
            </p>
          </Card>
          <Tabs value={tab} onChange={setTab} tabs={[{ value: 'containers', label: `Containers · ${d.containers.length}` }, { value: 'images', label: `Images · ${d.images.length}` }]} />
          {tab === 'containers' ? (
            <Card title="Containers">
              <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap', marginBottom: 12 }}>
                <Pill tone="ok">{sum.running} running</Pill>
                {sum.stopped ? <Pill tone="idle">{sum.stopped} stopped</Pill> : null}
                {sum.failing ? <Pill tone="bad">{sum.failing} failing</Pill> : null}
                <select className="fx-input" style={{ width: 'auto' }} aria-label="Show" value={group} onChange={(e) => setGroup(e.target.value as typeof group)}>
                  <option value="all">All containers</option><option value="running">Running</option><option value="stopped">Not running</option>
                </select>
                <input className="fx-input" style={{ flex: 1, minWidth: 180 }} aria-label="Filter containers" placeholder="Filter by name, image, id or status" value={q} onChange={(e) => setQ(e.target.value)} />
              </div>
              {shown.length === 0 ? (
                <EmptyState title={d.containers.length ? 'No container matches' : 'No containers'} body={d.containers.length ? 'Change the filter to see more.' : 'Run one with docker run; it shows here.'} />
              ) : (
                <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Name</th><th>Image</th><th>State</th><th>Status</th><th>Ports</th><th>Age</th><th /></tr></thead><tbody>
                  {shown.map((c) => (
                    <tr key={c.id}>
                      <td className="vm-name">{c.name}<div className="fx-muted fx-mono">{c.id}</div></td>
                      <td className="fx-mono">{c.image}</td>
                      <td><Pill tone={containerTone(c)}>{c.state}</Pill></td>
                      <td className="fx-muted">{c.status}</td>
                      <td className="fx-mono">{portsText(c.ports) || '—'}</td>
                      <td className="fx-muted">{ageSince(c.created, now)}</td>
                      <td><span className="fx-row-actions"><Button size="sm" variant="ghost" aria-label={`Logs of ${c.name}`} onClick={() => setOpen(c)}><ScrollText size={14} />Logs</Button></span></td>
                    </tr>
                  ))}
                </tbody></table></div>
              )}
            </Card>
          ) : (
            <Card title="Images">
              {d.images.length === 0 ? <EmptyState title="No images" body="Pull one with docker pull; it shows here." /> : (
                <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Image</th><th>Id</th><th>Size</th><th>Containers</th><th>Age</th></tr></thead><tbody>
                  {d.images.map((i) => (
                    <tr key={i.id}>
                      <td className="vm-name">{i.tags.length ? i.tags.map((t) => <div key={t} className="fx-mono">{t}</div>) : <span className="fx-muted">untagged</span>}</td>
                      <td className="fx-mono fx-muted">{i.id}</td>
                      <td>{fmtSize(i.size)}</td>
                      <td className="fx-muted">{i.containers > 0 ? i.containers : '—'}</td>
                      <td className="fx-muted">{ageSince(i.created, now)}</td>
                    </tr>
                  ))}
                </tbody></table></div>
              )}
            </Card>
          )}
        </>
      ) : null}
      {open ? <ContainerWindow container={open} onClose={() => setOpen(null)} /> : null}
    </>
  );
}
