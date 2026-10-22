import { useState, type ReactNode } from 'react';
import { Copy, Lock, LockOpen, Plus, Trash2 } from 'lucide-react';
import { Button, Card, EmptyState, Field, Hero, Pill, Sheet, Skeleton, Tabs, ago, api, navigate, usePoll } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { useAct } from '../lib/act';
import { rowsOf } from '../lib/generic';
import { fluxctlFor, isSource, nameProblem, sizeProblem } from '../lib/images';

type Tab = 'catalog' | 'oci' | 'templates' | 'pools';
type Row = Record<string, unknown>;
const str = (v: unknown): string => (typeof v === 'string' ? v : '');

export function CopyCmd({ cmd, notify }: { cmd: string; notify: Notify }) {
  return (
    <Button size="sm" variant="ghost" aria-label="Copy as fluxctl" title={cmd} onClick={() => navigator.clipboard.writeText(cmd).then(() => notify('Copied: ' + cmd), () => notify('Copy blocked by the browser', 'bad'))}>
      <Copy size={14} />
    </Button>
  );
}

function Confirm({ what, onConfirm, onClose, busy, verb = 'Delete' }: { what: string | null; onConfirm: () => void; onClose: () => void; busy: boolean; verb?: string }) {
  return (
    <Sheet open={!!what} title={`${verb} ${what ?? ''}`} onClose={onClose}>
      <p>{verb} <b>{what}</b>? This cannot be undone.</p>
      <div className="fx-actions"><Button variant="danger" busy={busy} onClick={onConfirm}>{verb}</Button><Button onClick={onClose}>Cancel</Button></div>
    </Sheet>
  );
}

function Table({ head, children, empty }: { head: string[]; children: ReactNode; empty?: ReactNode }) {
  return empty ? <>{empty}</> : (
    <div className="fx-table-wrap"><table className="fx-table"><thead><tr>{head.map((h) => <th key={h}>{h}</th>)}<th /></tr></thead><tbody>{children}</tbody></table></div>
  );
}

function CatalogTab({ notify }: { notify: Notify }) {
  const list = usePoll(async () => rowsOf(await api.get<unknown>('/v1/images/catalog'), 'items'), 10000);
  const { busy, run } = useAct(notify);
  const [add, setAdd] = useState(false);
  const [name, setName] = useState('');
  const [source, setSource] = useState('');
  const [format, setFormat] = useState('qcow2');
  const [touched, setTouched] = useState(false);
  const [del, setDel] = useState<string | null>(null);
  const [clone, setClone] = useState<string | null>(null);
  const [target, setTarget] = useState('');
  const rows = list.data ?? [];
  const problem = nameProblem(name) || (isSource(source) ? '' : 'Use an https:// URL or an absolute path on the node.');
  const submit = async () => {
    setTouched(true);
    if (problem) return;
    if (await run('add', () => api.post('/v1/images/catalog', { name, source, format }), `${name} added`, list.refresh)) (setAdd(false), setName(''), setSource(''), setTouched(false));
  };
  return (
    <>
      <div className="fx-actions" style={{ justifyContent: 'space-between' }}>
        <p className="fx-muted" style={{ margin: 0 }}>Named, checksum-pinned base images. Machines reference them by name.</p>
        <span className="fx-row-actions">
          <Button onClick={() => run('clean', () => api.post('/v1/images/catalog/clean', {}), 'Catalog cleaned', list.refresh)} busy={busy === 'clean'}>Clean up</Button>
          <Button variant="primary" onClick={() => setAdd(true)}><Plus size={16} />Add image</Button>
        </span>
      </div>
      <Card>
        {list.loading && !rows.length ? <Skeleton h={140} /> : (
          <Table head={['Name', 'Format', 'Distro', 'Signature', 'Checksum']} empty={rows.length === 0 ? <EmptyState title="The catalog is empty" body="Add a cloud image by URL or path. FluxVM downloads it and pins its checksum." action={<Button variant="primary" onClick={() => setAdd(true)}>Add an image</Button>} /> : undefined}>
            {rows.map((r) => {
              const n = str(r.name);
              const locked = r.read_only === true;
              return (
                <tr key={n}>
                  <td className="vm-name">{n} {locked ? <Pill tone="info" dot={false}>locked</Pill> : null}</td>
                  <td className="fx-muted">{str(r.format) || '—'}</td>
                  <td className="fx-muted">{[str(r.distro), str(r.version)].filter(Boolean).join(' ') || '—'}</td>
                  <td>{r.signature_valid === true ? <Pill tone="ok">verified</Pill> : r.signature_valid === false ? <Pill tone="bad">invalid</Pill> : <Pill tone="idle">unsigned</Pill>}</td>
                  <td className="fx-mono fx-muted">{str(r.sha256).slice(0, 12) || '—'}</td>
                  <td>
                    <span className="fx-row-actions">
                      <Button size="sm" variant="ghost" aria-label={locked ? `Unlock ${n}` : `Lock ${n}`} busy={busy === 'ro' + n} onClick={() => run('ro' + n, () => api.post(`/v1/images/catalog/${encodeURIComponent(n)}/read-only`, { read_only: !locked }), locked ? `${n} unlocked` : `${n} locked`, list.refresh)}>{locked ? <LockOpen size={14} /> : <Lock size={14} />}</Button>
                      <Button size="sm" variant="ghost" onClick={() => (setClone(n), setTarget(''))}>Clone</Button>
                      <CopyCmd notify={notify} cmd={fluxctlFor('catalog remove', n)} />
                      <Button size="sm" variant="ghost" aria-label={`Delete ${n}`} disabled={locked} title={locked ? 'Unlock it first' : undefined} onClick={() => setDel(n)}><Trash2 size={14} /></Button>
                    </span>
                  </td>
                </tr>
              );
            })}
          </Table>
        )}
      </Card>
      <Sheet open={add} title="Add an image" onClose={() => setAdd(false)}>
        <Field label="Name" hint="What machines will call it, e.g. ubuntu-24.04">{(id) => <input id={id} className="fx-input" value={name} onChange={(e) => setName(e.target.value)} />}</Field>
        <Field label="Source" hint="URL or absolute path. The checksum is computed from what lands on disk.">{(id) => <input id={id} className="fx-input" value={source} onChange={(e) => setSource(e.target.value)} placeholder="https://…/image.qcow2" />}</Field>
        <Field label="Format">{(id) => <select id={id} className="fx-input" value={format} onChange={(e) => setFormat(e.target.value)}><option>qcow2</option><option>raw</option></select>}</Field>
        {touched && problem ? <p className="fx-error" role="alert">{problem}</p> : null}
        <div className="fx-actions"><Button variant="primary" busy={busy === 'add'} onClick={submit}>Add and download</Button><Button onClick={() => setAdd(false)}>Cancel</Button></div>
      </Sheet>
      <Sheet open={!!clone} title={`Clone ${clone ?? ''}`} onClose={() => setClone(null)}>
        <Field label="New name">{(id) => <input id={id} className="fx-input" value={target} onChange={(e) => setTarget(e.target.value)} />}</Field>
        {target && nameProblem(target) ? <p className="fx-error" role="alert">{nameProblem(target)}</p> : null}
        <div className="fx-actions"><Button variant="primary" busy={busy === 'clone'} disabled={!!nameProblem(target)} onClick={async () => { if (clone && (await run('clone', () => api.post(`/v1/images/catalog/${encodeURIComponent(clone)}/clone`, { target_name: target }), `${target} created`, list.refresh))) setClone(null); }}>Clone</Button><Button onClick={() => setClone(null)}>Cancel</Button></div>
      </Sheet>
      <Confirm what={del} busy={busy === 'del'} onClose={() => setDel(null)} onConfirm={async () => { if (del && (await run('del', () => api.del(`/v1/images/catalog/${encodeURIComponent(del)}`), `${del} removed`, list.refresh))) setDel(null); }} />
    </>
  );
}

function OciTab({ notify, macos }: { notify: Notify; macos: boolean }) {
  const list = usePoll(async () => rowsOf(await api.get<unknown>('/v1/oci/images').catch(() => ({ items: [] })), 'items'), 10000);
  const { busy, run } = useAct(notify);
  const [image, setImage] = useState('');
  const [platform, setPlatform] = useState('');
  const [del, setDel] = useState<string | null>(null);
  const rows = list.data ?? [];
  return (
    <>
      {!macos ? <p className="fx-muted">OCI rootfs images back <code>vz</code> sandboxes, which run on macOS hosts. On this node they are listed but not used.</p> : null}
      <Card title="Pull an image">
        <div className="fx-actions">
          <input className="fx-input" aria-label="Image reference" value={image} onChange={(e) => setImage(e.target.value)} placeholder="alpine:3.22" style={{ maxWidth: 340 }} />
          <select className="fx-input" aria-label="Platform" value={platform} onChange={(e) => setPlatform(e.target.value)} style={{ maxWidth: 160 }}><option value="">default</option><option>linux/arm64</option><option>linux/amd64</option></select>
          <Button variant="primary" disabled={!image.trim()} busy={busy === 'pull'} onClick={async () => { if (await run('pull', () => api.post('/v1/oci/images', { image: image.trim(), ...(platform ? { platform } : {}) }), `${image.trim()} pulled`, list.refresh)) setImage(''); }}>Pull</Button>
          <CopyCmd notify={notify} cmd={fluxctlFor('oci pull', image.trim() || 'IMAGE')} />
        </div>
      </Card>
      <div className="fx-section">
        <Card title="Cached images" action={<Button size="sm" busy={busy === 'prune'} onClick={() => run('prune', () => api.post('/v1/oci/prune', {}), 'Unused images pruned', list.refresh)}>Prune unused</Button>}>
          {list.loading && !rows.length ? <Skeleton h={100} /> : (
            <Table head={['Reference', 'Digest', 'Used']} empty={rows.length === 0 ? <EmptyState title="Nothing cached" body="Pulled images are unpacked once and reused by every sandbox." /> : undefined}>
              {rows.map((r, i) => {
                const ref = str(r.reference) || str(r.image) || str(r.name);
                const digest = str(r.digest);
                return (
                  <tr key={digest || i}>
                    <td className="vm-name">{ref || '—'}</td>
                    <td className="fx-mono fx-muted">{digest.replace('sha256:', '').slice(0, 12) || '—'}</td>
                    <td className="fx-muted">{typeof r.last_used === 'string' ? ago(r.last_used) : '—'}</td>
                    <td><span className="fx-row-actions"><Button size="sm" variant="ghost" aria-label={`Remove ${ref}`} onClick={() => setDel(digest || ref)}><Trash2 size={14} /></Button></span></td>
                  </tr>
                );
              })}
            </Table>
          )}
        </Card>
      </div>
      <Confirm what={del} verb="Remove" busy={busy === 'rm'} onClose={() => setDel(null)} onConfirm={async () => { if (del && (await run('rm', () => api.del(`/v1/oci/images/${encodeURIComponent(del)}`), 'Image removed', list.refresh))) setDel(null); }} />
    </>
  );
}

function TemplatesTab({ notify }: { notify: Notify }) {
  const list = usePoll(async () => rowsOf(await api.get<unknown>('/v1/vm-templates'), 'items', 'templates'), 10000);
  const { busy, run } = useAct(notify);
  const [use, setUse] = useState<string | null>(null);
  const [vmName, setVmName] = useState('');
  const [del, setDel] = useState<string | null>(null);
  const rows = list.data ?? [];
  return (
    <>
      <p className="fx-muted">Named machine specs: save one, create many. Save a template from a machine's header or <code>fluxctl vm-template save</code>.</p>
      <Card>
        {list.loading && !rows.length ? <Skeleton h={120} /> : (
          <Table head={['Name', 'Description', 'Saved']} empty={rows.length === 0 ? <EmptyState title="No templates yet" body="Describe a machine once and stamp out copies in a second." /> : undefined}>
            {rows.map((r) => {
              const n = str(r.name);
              return (
                <tr key={n}>
                  <td className="vm-name">{n}</td>
                  <td className="fx-muted">{str(r.description) || '—'}</td>
                  <td className="fx-muted">{str(r.created_at) ? ago(str(r.created_at)) : '—'}</td>
                  <td><span className="fx-row-actions">
                    <Button size="sm" variant="primary" onClick={() => (setUse(n), setVmName(''))}>Create machine</Button>
                    <CopyCmd notify={notify} cmd={fluxctlFor('vm-template create', n, 'NAME')} />
                    <Button size="sm" variant="ghost" aria-label={`Delete ${n}`} onClick={() => setDel(n)}><Trash2 size={14} /></Button>
                  </span></td>
                </tr>
              );
            })}
          </Table>
        )}
      </Card>
      <Sheet open={!!use} title={`Create from ${use ?? ''}`} onClose={() => setUse(null)}>
        <Field label="Machine name">{(id) => <input id={id} className="fx-input" value={vmName} onChange={(e) => setVmName(e.target.value)} />}</Field>
        {vmName && nameProblem(vmName) ? <p className="fx-error" role="alert">{nameProblem(vmName)}</p> : null}
        <div className="fx-actions"><Button variant="primary" busy={busy === 'inst'} disabled={!!nameProblem(vmName)} onClick={async () => {
          if (!use) return;
          let id = '';
          const ok = await run('inst', async () => { id = str((await api.post<Row>(`/v1/vm-templates/${encodeURIComponent(use)}/instantiate`, { name: vmName })).id); }, `${vmName} created`);
          if (ok) (setUse(null), id ? navigate(`/machines/${id}`) : navigate('/machines'));
        }}>Create</Button><Button onClick={() => setUse(null)}>Cancel</Button></div>
      </Sheet>
      <Confirm what={del} busy={busy === 'tdel'} onClose={() => setDel(null)} onConfirm={async () => { if (del && (await run('tdel', () => api.del(`/v1/vm-templates/${encodeURIComponent(del)}`), `${del} deleted`, list.refresh))) setDel(null); }} />
    </>
  );
}

function PoolsTab({ notify }: { notify: Notify }) {
  const list = usePoll(async () => rowsOf(await api.get<unknown>('/v1/pools'), 'items', 'pools'), 5000);
  const templates = usePoll(async () => rowsOf(await api.get<unknown>('/v1/vm-templates').catch(() => ({ items: [] })), 'items', 'templates'), 60000);
  const { busy, run } = useAct(notify);
  const [create, setCreate] = useState(false);
  const [name, setName] = useState('');
  const [tpl, setTpl] = useState('');
  const [size, setSize] = useState('2');
  const [touched, setTouched] = useState(false);
  const [resize, setResize] = useState<{ name: string; size: string } | null>(null);
  const [del, setDel] = useState<string | null>(null);
  const rows = list.data ?? [];
  const tpls = templates.data ?? [];
  const problem = nameProblem(name) || (tpl ? '' : 'Pick a template to keep warm.') || sizeProblem(size);
  const submit = async () => {
    setTouched(true);
    if (problem) return;
    const ok = await run('pcreate', async () => {
      const t = await api.get<Row>(`/v1/vm-templates/${encodeURIComponent(tpl)}`);
      await api.post('/v1/pools', { name, size: Number(size), template: t.spec });
    }, `${name} created`, list.refresh);
    if (ok) (setCreate(false), setName(''), setTouched(false));
  };
  return (
    <>
      <div className="fx-actions" style={{ justifyContent: 'space-between' }}>
        <p className="fx-muted" style={{ margin: 0 }}>Pre-booted, paused machines. Claiming one takes about as long as a resume.</p>
        <Button variant="primary" onClick={() => setCreate(true)}><Plus size={16} />New pool</Button>
      </div>
      <Card>
        {list.loading && !rows.length ? <Skeleton h={120} /> : (
          <Table head={['Pool', 'Ready', 'Target', 'Filling']} empty={rows.length === 0 ? <EmptyState title="No pools" body="A pool keeps a few machines warm from a template so a claim is nearly instant." /> : undefined}>
            {rows.map((r) => {
              const n = str(r.name);
              return (
                <tr key={n}>
                  <td className="vm-name">{n}</td>
                  <td><Pill tone={Number(r.ready) > 0 ? 'ok' : 'warn'}>{String(r.ready ?? 0)}</Pill></td>
                  <td className="fx-muted">{String(r.size ?? '—')}</td>
                  <td className="fx-muted">{Number(r.pending) > 0 ? `${r.pending} to go` : 'full'}</td>
                  <td><span className="fx-row-actions">
                    <Button size="sm" variant="primary" busy={busy === 'claim' + n} onClick={async () => {
                      let id = '';
                      if (await run('claim' + n, async () => { id = str((await api.post<Row>(`/v1/pools/${encodeURIComponent(n)}/claim`, {})).id); }, 'Claimed a machine', list.refresh)) navigate(id ? `/machines/${id}` : '/machines');
                    }}>Claim</Button>
                    <Button size="sm" variant="ghost" onClick={() => setResize({ name: n, size: String(r.size ?? 1) })}>Resize</Button>
                    <CopyCmd notify={notify} cmd={fluxctlFor('pool claim', n)} />
                    <Button size="sm" variant="ghost" aria-label={`Delete ${n}`} onClick={() => setDel(n)}><Trash2 size={14} /></Button>
                  </span></td>
                </tr>
              );
            })}
          </Table>
        )}
      </Card>
      <Sheet open={create} title="New pool" onClose={() => setCreate(false)}>
        <Field label="Name">{(id) => <input id={id} className="fx-input" value={name} onChange={(e) => setName(e.target.value)} />}</Field>
        <Field label="Template" hint={tpls.length ? undefined : 'Save a template first; a pool is stamped from one.'}>{(id) => <select id={id} className="fx-input" value={tpl} onChange={(e) => setTpl(e.target.value)}><option value="">Choose…</option>{tpls.map((t) => <option key={str(t.name)}>{str(t.name)}</option>)}</select>}</Field>
        <Field label="Warm machines">{(id) => <input id={id} className="fx-input" inputMode="numeric" value={size} onChange={(e) => setSize(e.target.value)} />}</Field>
        {touched && problem ? <p className="fx-error" role="alert">{problem}</p> : null}
        <div className="fx-actions"><Button variant="primary" busy={busy === 'pcreate'} onClick={submit}>Create pool</Button><Button onClick={() => setCreate(false)}>Cancel</Button></div>
      </Sheet>
      <Sheet open={!!resize} title={`Resize ${resize?.name ?? ''}`} onClose={() => setResize(null)}>
        <Field label="Warm machines" hint="Shrinking trims ready machines; growing boots more.">{(id) => <input id={id} className="fx-input" inputMode="numeric" value={resize?.size ?? ''} onChange={(e) => resize && setResize({ ...resize, size: e.target.value })} />}</Field>
        {resize && sizeProblem(resize.size) ? <p className="fx-error" role="alert">{sizeProblem(resize.size)}</p> : null}
        <div className="fx-actions"><Button variant="primary" busy={busy === 'rs'} disabled={!resize || !!sizeProblem(resize.size)} onClick={async () => { if (resize && (await run('rs', () => api.post(`/v1/pools/${encodeURIComponent(resize.name)}/resize`, { size: Number(resize.size) }), 'Pool resized', list.refresh))) setResize(null); }}>Resize</Button><Button onClick={() => setResize(null)}>Cancel</Button></div>
      </Sheet>
      <Confirm what={del} busy={busy === 'pdel'} onClose={() => setDel(null)} onConfirm={async () => { if (del && (await run('pdel', () => api.del(`/v1/pools/${encodeURIComponent(del)}`), `${del} deleted`, list.refresh))) setDel(null); }} />
    </>
  );
}

export function ImagesPage({ notify, macos }: { notify: Notify; macos: boolean }) {
  const [tab, setTab] = useState<Tab>('catalog');
  return (
    <>
      <Hero eyebrow="Images" title="Start from something" accent="you trust." lede="Checksum-pinned base images, reusable machine templates and warm pools that make a new machine instant." />
      <Tabs<Tab> value={tab} onChange={setTab} tabs={[{ value: 'catalog', label: 'Catalog' }, { value: 'templates', label: 'Templates' }, { value: 'pools', label: 'Pools' }, { value: 'oci', label: 'OCI images' }]} />
      <div className="fx-section">
        {tab === 'catalog' ? <CatalogTab notify={notify} /> : tab === 'templates' ? <TemplatesTab notify={notify} /> : tab === 'pools' ? <PoolsTab notify={notify} /> : <OciTab notify={notify} macos={macos} />}
      </div>
    </>
  );
}
