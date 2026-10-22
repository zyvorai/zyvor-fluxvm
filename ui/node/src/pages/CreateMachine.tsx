import { useEffect, useMemo, useState } from 'react';
import { ArrowLeft, ArrowRight, Plus, Trash2 } from 'lucide-react';
import { Button, Field, Segmented, Sheet, Steps, api, fmtMiB, navigate, type Vm } from '@fluxvm/ui-core';
import type { Notify } from '../App';

interface CatalogEntry {
  name: string;
  distro?: string | null;
  version?: string | null;
}
type NetMode = 'user' | 'none' | 'tap';
interface Fwd {
  host_port: string;
  guest_port: string;
}

export const SIZES = [
  { id: 'small', label: 'Small', vcpus: 1, memory_mib: 1024, disk: 10 },
  { id: 'medium', label: 'Medium', vcpus: 2, memory_mib: 4096, disk: 20 },
  { id: 'large', label: 'Large', vcpus: 4, memory_mib: 8192, disk: 40 },
  { id: 'xl', label: 'X-Large', vcpus: 8, memory_mib: 16384, disk: 80 },
] as const;

const NAME_RE = /^[a-zA-Z0-9][a-zA-Z0-9._-]{0,62}$/;

export interface Draft {
  name: string;
  image: string;
  size: string;
  vcpus: number;
  memory_mib: number;
  disk: number;
  net: NetMode;
  forwards: Fwd[];
  bridge: string;
  hostname: string;
  user: string;
  ssh: string;
}

export const emptyDraft = (): Draft => ({ name: '', image: '', size: 'medium', vcpus: 2, memory_mib: 4096, disk: 20, net: 'user', forwards: [], bridge: '', hostname: '', user: '', ssh: '' });

/** Maps the wizard state to the `POST /v1/vms` body. Pure so it can be unit-tested. */
export function buildRequest(d: Draft): Record<string, unknown> {
  const req: Record<string, unknown> = { name: d.name.trim(), backend: 'auto', image: d.image.trim(), vcpus: d.vcpus, memory_mib: d.memory_mib, disk_size_gib: d.disk };
  if (d.net === 'none') req.network = { mode: 'none' };
  else if (d.net === 'tap') req.network = { mode: 'tap', ...(d.bridge.trim() ? { bridge: d.bridge.trim() } : {}) };
  else
    req.network = {
      mode: 'user',
      forwards: d.forwards.filter((f) => f.host_port && f.guest_port).map((f) => ({ host_port: Number(f.host_port), guest_port: Number(f.guest_port), protocol: 'tcp' })),
    };
  const keys = d.ssh.split('\n').map((k) => k.trim()).filter(Boolean);
  if (d.hostname.trim() || d.user.trim() || keys.length) {
    req.cloud_init = { ...(d.hostname.trim() ? { hostname: d.hostname.trim() } : {}), ...(d.user.trim() ? { user: d.user.trim() } : {}), ssh_authorized_keys: keys };
  }
  return req;
}

export function validate(d: Draft, step: number): string {
  if (step === 0) {
    if (!NAME_RE.test(d.name.trim())) return 'Name: letters, digits, dot, dash or underscore (up to 63).';
    if (!d.image.trim()) return 'Pick an image or enter a path.';
  }
  if (step === 1 && (d.vcpus < 1 || d.memory_mib < 256 || d.disk < 1)) return 'Size must be at least 1 vCPU, 256 MiB and 1 GiB.';
  if (step === 2 && d.net === 'user') {
    for (const f of d.forwards) {
      const h = Number(f.host_port);
      const g = Number(f.guest_port);
      if (!(h >= 1 && h <= 65535 && g >= 1 && g <= 65535)) return 'Port forwards need ports between 1 and 65535.';
    }
  }
  return '';
}

const STEPS = ['Image', 'Size', 'Network', 'Review'];

export function CreateMachine({ open, onClose, notify, onCreated }: { open: boolean; onClose: () => void; notify: Notify; onCreated: () => void }) {
  const [d, setD] = useState<Draft>(emptyDraft);
  const [step, setStep] = useState(0);
  const [catalog, setCatalog] = useState<CatalogEntry[]>([]);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState('');
  useEffect(() => {
    if (!open) return;
    setD(emptyDraft());
    setStep(0);
    setErr('');
    api.get<{ items: CatalogEntry[] }>('/v1/images/catalog').then((r) => setCatalog(r.items ?? [])).catch(() => setCatalog([]));
  }, [open]);
  const set = <K extends keyof Draft>(k: K, v: Draft[K]) => setD((x) => ({ ...x, [k]: v }));
  const body = useMemo(() => buildRequest(d), [d]);
  const problem = validate(d, step);
  // Don't greet an untouched form with an error: show it once the user has started filling it in.
  const showProblem = !!problem && (step > 0 || !!d.name || !!d.image);

  const submit = async () => {
    setBusy(true);
    setErr('');
    try {
      const vm = await api.post<Vm>('/v1/vms', body);
      notify(`${d.name} is being created`);
      onCreated();
      onClose();
      if (vm?.id) navigate(`/machines/${vm.id}`);
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Sheet open={open} title="New machine" onClose={onClose}>
      <Steps steps={STEPS} index={step} />
      {step === 0 && (
        <>
          <Field label="Name" hint="Used for the VM and its hostname by default.">{(id) => <input id={id} className="fx-input" autoFocus value={d.name} onChange={(e) => set('name', e.target.value)} placeholder="web-01" />}</Field>
          <Field label="Image" hint={catalog.length ? 'From the image catalog, or type a path / URL alias.' : 'No catalog entries found — enter an image path or alias.'}>
            {(id) => (
              <>
                {catalog.length ? (
                  <select id={id} className="fx-input" value={catalog.some((c) => c.name === d.image) ? d.image : ''} onChange={(e) => set('image', e.target.value)}>
                    <option value="">Choose from catalog…</option>
                    {catalog.map((c) => <option key={c.name} value={c.name}>{c.name}{c.distro ? ` — ${c.distro}${c.version ? ' ' + c.version : ''}` : ''}</option>)}
                  </select>
                ) : null}
                <input className="fx-input" style={{ marginTop: catalog.length ? 8 : 0 }} value={d.image} onChange={(e) => set('image', e.target.value)} placeholder="ubuntu-24.04 or /var/lib/fluxvm/images/base.qcow2" />
              </>
            )}
          </Field>
        </>
      )}
      {step === 1 && (
        <>
          <div className="fx-presets">
            {SIZES.map((s) => (
              <button key={s.id} className="fx-preset" aria-pressed={d.size === s.id} onClick={() => setD((x) => ({ ...x, size: s.id, vcpus: s.vcpus, memory_mib: s.memory_mib, disk: s.disk }))}>
                <b>{s.label}</b>
                <small>{s.vcpus} vCPU · {fmtMiB(s.memory_mib)} · {s.disk} GiB</small>
              </button>
            ))}
          </div>
          <Field label="vCPUs">{(id) => <input id={id} className="fx-input" type="number" min={1} max={255} value={d.vcpus} onChange={(e) => setD((x) => ({ ...x, size: 'custom', vcpus: Number(e.target.value) }))} />}</Field>
          <Field label="Memory (MiB)">{(id) => <input id={id} className="fx-input" type="number" min={256} step={256} value={d.memory_mib} onChange={(e) => setD((x) => ({ ...x, size: 'custom', memory_mib: Number(e.target.value) }))} />}</Field>
          <Field label="Disk (GiB)">{(id) => <input id={id} className="fx-input" type="number" min={1} value={d.disk} onChange={(e) => setD((x) => ({ ...x, size: 'custom', disk: Number(e.target.value) }))} />}</Field>
        </>
      )}
      {step === 2 && (
        <>
          <Field label="Networking">
            {() => (
              <Segmented label="Network mode" value={d.net} onChange={(v) => set('net', v)} options={[{ value: 'user', label: 'NAT (user-mode)' }, { value: 'tap', label: 'Bridged' }, { value: 'none', label: 'None' }]} />
            )}
          </Field>
          {d.net === 'user' && (
            <Field label="Port forwards" hint="Expose guest ports on the host. Outbound access is governed by the egress policy.">
              {() => (
                <div style={{ display: 'grid', gap: 8 }}>
                  {d.forwards.map((f, i) => (
                    <div key={i} style={{ display: 'flex', gap: 8 }}>
                      <input className="fx-input" aria-label="Host port" placeholder="host 8080" value={f.host_port} onChange={(e) => set('forwards', d.forwards.map((x, j) => (j === i ? { ...x, host_port: e.target.value } : x)))} />
                      <input className="fx-input" aria-label="Guest port" placeholder="guest 80" value={f.guest_port} onChange={(e) => set('forwards', d.forwards.map((x, j) => (j === i ? { ...x, guest_port: e.target.value } : x)))} />
                      <Button size="sm" variant="ghost" aria-label="Remove forward" onClick={() => set('forwards', d.forwards.filter((_, j) => j !== i))}><Trash2 size={14} /></Button>
                    </div>
                  ))}
                  <div><Button size="sm" onClick={() => set('forwards', [...d.forwards, { host_port: '', guest_port: '' }])}><Plus size={14} />Add forward</Button></div>
                </div>
              )}
            </Field>
          )}
          {d.net === 'tap' && <Field label="Bridge" hint="Leave empty for the daemon's default bridge.">{(id) => <input id={id} className="fx-input" value={d.bridge} onChange={(e) => set('bridge', e.target.value)} placeholder="br0" />}</Field>}
          <Field label="Hostname" hint="Optional cloud-init.">{(id) => <input id={id} className="fx-input" value={d.hostname} onChange={(e) => set('hostname', e.target.value)} />}</Field>
          <Field label="User">{(id) => <input id={id} className="fx-input" value={d.user} onChange={(e) => set('user', e.target.value)} placeholder="ubuntu" />}</Field>
          <Field label="SSH public keys" hint="One per line.">{(id) => <textarea id={id} className="fx-input" value={d.ssh} onChange={(e) => set('ssh', e.target.value)} />}</Field>
        </>
      )}
      {step === 3 && (
        <>
          <p className="fx-muted">This is exactly what will be sent to <code>POST /v1/vms</code>.</p>
          <pre className="fx-code">{JSON.stringify(body, null, 2)}</pre>
        </>
      )}
      {(showProblem && step < 3) || err ? <div className="fx-error" role="alert" style={{ marginTop: 12 }}>{err || problem}</div> : null}
      <div className="fx-actions" style={{ marginTop: 22, justifyContent: 'space-between' }}>
        <Button variant="ghost" disabled={step === 0} onClick={() => setStep(step - 1)}><ArrowLeft size={14} />Back</Button>
        {step < 3 ? (
          <Button variant="primary" disabled={!!problem} onClick={() => setStep(step + 1)}>Next<ArrowRight size={14} /></Button>
        ) : (
          <Button variant="primary" busy={busy} onClick={submit}>Create machine</Button>
        )}
      </div>
    </Sheet>
  );
}
