import { useState } from 'react';
import { Plus } from 'lucide-react';
import { Button, Card, EmptyState, Field, Hero, Pill, Sheet, Skeleton, api, fmtMiB, usePoll } from '@fluxvm/ui-core';
import type { Ctx } from '../App';
import { useAct } from '../lib/act';
import { rowsOf } from '../lib/generic';
import { type CatalogImage, type ImageDraft, draftOf, emptyImageDraft, imageProblem, imageRequest } from '../lib/images';

const enc = encodeURIComponent;

/** The image catalog (operator): what tenants can pick when they launch. */
export function ImagesPage({ notify }: Ctx) {
  const list = usePoll(async () => rowsOf(await api.get<unknown>('/fleet/images'), 'items') as unknown as CatalogImage[], 30000);
  const { busy, run } = useAct(notify);
  const [edit, setEdit] = useState<{ previous?: CatalogImage; draft: ImageDraft } | null>(null);
  const [touched, setTouched] = useState(false);
  const [del, setDel] = useState<CatalogImage | null>(null);
  const rows = list.data ?? [];
  const problem = edit ? imageProblem(edit.draft, !!edit.previous, rows.map((r) => r.name)) : '';
  const set = (patch: Partial<ImageDraft>) => edit && setEdit({ ...edit, draft: { ...edit.draft, ...patch } });
  const open = (previous?: CatalogImage) => (setTouched(false), setEdit({ previous, draft: previous ? draftOf(previous) : emptyImageDraft() }));
  return (
    <>
      <Hero eyebrow="Images" title="What tenants can" accent="launch." lede="The catalog lists disks that are already on your Macs. Add an image after you have put its disk there with fluxctl vm-image pull.">
        <Button variant="primary" size="lg" onClick={() => open()}><Plus size={18} />Add image</Button>
      </Hero>
      <Card>
        {list.loading && !rows.length ? <Skeleton h={120} /> : rows.length === 0 ? <EmptyState title="The catalog is empty" body="Add the images you have pulled onto your Macs, so tenants can launch them." action={<Button variant="primary" onClick={() => open()}>Add an image</Button>} /> : (
          <div className="fx-table-wrap"><table className="fx-table">
            <thead><tr><th>Image</th><th>Guest</th><th>Default size</th><th>Disk</th><th /></tr></thead>
            <tbody>
              {rows.map((i) => (
                <tr key={i.name}>
                  <td><div className="vm-name">{i.display_name || i.name}</div><div className="fx-muted fx-mono">{i.name}</div></td>
                  <td><Pill tone="idle" dot={false}>{i.guest_os === 'macos' ? 'macOS' : 'Linux'}</Pill></td>
                  <td className="fx-muted">{i.vcpus} vCPUs, {fmtMiB(i.memory_mib)}</td>
                  <td className="fx-muted fx-mono">{i.image}</td>
                  <td><div className="fx-actions"><Button size="sm" aria-label={`Edit ${i.display_name || i.name}`} onClick={() => open(i)}>Edit</Button><Button size="sm" variant="danger" aria-label={`Remove ${i.display_name || i.name}`} onClick={() => setDel(i)}>Remove</Button></div></td>
                </tr>
              ))}
            </tbody>
          </table></div>
        )}
      </Card>
      <Sheet open={!!edit} title={edit?.previous ? `Edit ${edit.previous.display_name || edit.previous.name}` : 'Add an image'} onClose={() => setEdit(null)}>
        {edit ? (
          <>
            <p className="fx-muted">The disk must already be at this path on every Mac that should run it.</p>
            <div className="fx-grid cols-2">
              <Field label="ID">{(id) => <input id={id} className="fx-input" value={edit.draft.name} readOnly={!!edit.previous} placeholder="macos-27-xcode" autoComplete="off" spellCheck={false} onChange={(e) => set({ name: e.target.value })} />}</Field>
              <Field label="Display name">{(id) => <input id={id} className="fx-input" value={edit.draft.displayName} placeholder="macOS 27 with Xcode" onChange={(e) => set({ displayName: e.target.value })} />}</Field>
              <Field label="Guest">{(id) => <select id={id} className="fx-input" value={edit.draft.guest} onChange={(e) => set({ guest: e.target.value as ImageDraft['guest'] })}><option value="macos">macOS</option><option value="linux">Linux</option></select>}</Field>
              <Field label="Source (optional)">{(id) => <input id={id} className="fx-input" value={edit.draft.source} placeholder="ghcr.io/acme/macos-xcode:27" onChange={(e) => set({ source: e.target.value })} />}</Field>
            </div>
            <Field label="Disk path on the Macs">{(id) => <input id={id} className="fx-input fx-mono" value={edit.draft.image} placeholder="/Users/fluxvm/vms/macos-27-xcode/disk.raw" autoComplete="off" spellCheck={false} onChange={(e) => set({ image: e.target.value })} />}</Field>
            <div className="fx-grid cols-2">
              <Field label="Default vCPUs">{(id) => <input id={id} className="fx-input" inputMode="numeric" value={edit.draft.vcpus} onChange={(e) => set({ vcpus: e.target.value })} />}</Field>
              <Field label="Default memory (GiB)">{(id) => <input id={id} className="fx-input" inputMode="decimal" value={edit.draft.memoryGib} onChange={(e) => set({ memoryGib: e.target.value })} />}</Field>
            </div>
            <Field label="Description (optional)">{(id) => <input id={id} className="fx-input" value={edit.draft.description} placeholder="Xcode 27, Homebrew and the GitHub runner" onChange={(e) => set({ description: e.target.value })} />}</Field>
            <Field label="SSH user (optional)">{(id) => <input id={id} className="fx-input" value={edit.draft.sshUser} placeholder="admin" autoComplete="off" spellCheck={false} onChange={(e) => set({ sshUser: e.target.value })} />}</Field>
            {touched && problem ? <p className="fx-error" role="alert">{problem}</p> : null}
            <div className="fx-actions"><Button variant="primary" busy={busy === 'save'} onClick={async () => {
              setTouched(true);
              if (problem) return;
              if (await run('save', () => api.put(`/fleet/images/${enc(edit.draft.name)}`, imageRequest(edit.draft, edit.previous)), `${edit.previous ? 'Saved' : 'Added'} ${edit.draft.displayName || edit.draft.name}`, list.refresh)) setEdit(null);
            }}>{edit.previous ? 'Save changes' : 'Add image'}</Button><Button onClick={() => setEdit(null)}>Cancel</Button></div>
          </>
        ) : null}
      </Sheet>
      <Sheet open={!!del} title={`Remove ${del?.display_name || del?.name || ''}`} onClose={() => setDel(null)}>
        <p>Remove <b>{del?.display_name || del?.name}</b> from the catalog? VMs already made from it keep running and the disk stays on the Macs. Tenants can no longer pick it.</p>
        <div className="fx-actions"><Button variant="danger" busy={busy === 'rm'} onClick={async () => { if (del && (await run('rm', () => api.del(`/fleet/images/${enc(del.name)}`), `${del.display_name || del.name} removed`, list.refresh))) setDel(null); }}>Remove image</Button><Button onClick={() => setDel(null)}>Cancel</Button></div>
      </Sheet>
    </>
  );
}
