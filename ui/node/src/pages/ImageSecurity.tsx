// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { Copy, Download } from 'lucide-react';
import { Button, Card, EmptyState, Pill, Skeleton, api, usePoll } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { useAct } from '../lib/act';
import {
  type Assessment, type SecurityView, POLICY_INIT_COMMAND, UPDATE_COMMAND, advisoryLine, badgeTone, badgeWord, countsLine, nextCommand,
  sbomCommand, sbomPath, scanCommand, signatureWord, sortAssessments, verifyCommand,
} from '../lib/imageSecurity';

function Copy1({ cmd, notify, label = 'Copy as fluxctl' }: { cmd: string; notify: Notify; label?: string }) {
  return (
    <Button size="sm" variant="ghost" aria-label={label} title={cmd} onClick={() => navigator.clipboard.writeText(cmd).then(() => notify('Copied: ' + cmd), () => notify('Copy blocked by the browser', 'bad'))}>
      <Copy size={14} />
    </Button>
  );
}

async function saveSbom(a: Assessment): Promise<void> {
  const doc = await api.get<unknown>(sbomPath(a.manifest_digest));
  const url = URL.createObjectURL(new Blob([JSON.stringify(doc, null, 2)], { type: 'application/json' }));
  const el = document.createElement('a');
  el.href = url;
  el.download = `sbom-${a.manifest_digest.replace('sha256:', '').slice(0, 12)}.spdx.json`;
  el.click();
  URL.revokeObjectURL(url);
}

/** Verdict badges per cached OCI image: signed, known problems, would the policy let it start. Read-only: the checks that
 *  change anything are fluxctl commands, offered with Copy as fluxctl. */
export function ImageSecurityTab({ notify }: { notify: Notify }) {
  const view = usePoll(async () => api.get<SecurityView>('/v1/oci/security'), 15000);
  const { busy, run } = useAct(notify);
  const v = view.data;
  if (view.loading && !v) return <Skeleton h={120} />;
  const rows = sortAssessments(v?.items ?? []);
  return (
    <>
      <Card title="Policy and advisory data">
        <p>{v?.policy.present ? 'An image policy is on: it is checked every time a container sandbox starts.' : 'No image policy yet, so nothing is blocked.'}{v?.policy.error ? <span className="fx-error"> {v.policy.error}</span> : null}</p>
        <p className="fx-muted">{advisoryLine(v?.advisory)}</p>
        <div className="fx-actions">
          {v?.policy.present ? null : <><code>{POLICY_INIT_COMMAND}</code><Copy1 cmd={POLICY_INIT_COMMAND} notify={notify} /></>}
          <code>{UPDATE_COMMAND}</code><Copy1 cmd={UPDATE_COMMAND} notify={notify} />
        </div>
        {v?.overrides.length ? <p className="fx-error" role="alert">{v.overrides.length} recent start(s) went ahead against the policy (latest: {v.overrides[v.overrides.length - 1].reference} by {v.overrides[v.overrides.length - 1].actor}). <code>fluxctl image policy overrides</code></p> : null}
      </Card>
      <div className="fx-section">
        <Card title="Cached images">
          {rows.length === 0 ? <EmptyState title="Nothing to check yet" body="Pull an image on the OCI images tab, then its signature, packages and known problems show here." /> : (
            <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Image</th><th>Verdict</th><th>Signature</th><th>Known problems</th><th>Packages</th><th /></tr></thead><tbody>
              {rows.map((a) => (
                <tr key={a.manifest_digest}>
                  <td className="vm-name" title={a.manifest_digest}>{a.reference}<div className="fx-muted">{a.summary}</div></td>
                  <td><Pill tone={badgeTone(a.badge)}>{badgeWord(a.badge)}</Pill></td>
                  <td>{signatureWord(a.signature)}</td>
                  <td>{a.scan?.advisory.present ? countsLine(a.scan.counts) : 'not scanned'}</td>
                  <td>{a.packages ?? '—'}</td>
                  <td><span className="fx-row-actions">
                    <Button size="sm" variant="ghost" aria-label={`Download SBOM for ${a.reference}`} title="Download the SBOM (SPDX)" busy={busy === 'sbom' + a.manifest_digest} onClick={() => run('sbom' + a.manifest_digest, () => saveSbom(a), 'SBOM saved')}><Download size={14} /></Button>
                    <Copy1 cmd={nextCommand(a)} notify={notify} />
                    <Copy1 cmd={verifyCommand(a)} notify={notify} label="Copy verify command" />
                    <Copy1 cmd={scanCommand(a)} notify={notify} label="Copy scan command" />
                    <Copy1 cmd={sbomCommand(a)} notify={notify} label="Copy SBOM command" />
                  </span></td>
                </tr>
              ))}
            </tbody></table></div>
          )}
        </Card>
      </div>
    </>
  );
}
