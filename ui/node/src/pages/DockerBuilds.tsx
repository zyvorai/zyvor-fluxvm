// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { Copy } from 'lucide-react';
import { Button, Card, EmptyState, Pill, Skeleton, ago, api, usePoll } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import {
  type DockerBuildsView, CONTEXT_COMMAND, HISTORY_COMMAND, buildLabel, buildTone, builderNote, durationText, errorLine, shortImage, sizeText, summarizeBuilds, tagsText,
} from '../lib/dockerBuilds';

function Copy1({ cmd, notify }: { cmd: string; notify: Notify }) {
  return (
    <Button size="sm" variant="ghost" aria-label="Copy as fluxctl" title={cmd} onClick={() => navigator.clipboard.writeText(cmd).then(() => notify('Copied: ' + cmd), () => notify('Copy blocked by the browser', 'bad'))}>
      <Copy size={14} />
    </Button>
  );
}

/** What `docker build` made through the FluxVM Docker socket: name, outcome, time, image id and the fluxctl command that repeats it.
 *  Read-only: a build is started with `docker build` (or `fluxctl mac build`), so the commands are offered with Copy as fluxctl. */
export function DockerBuildsTab({ notify }: { notify: Notify }) {
  const view = usePoll(async () => api.get<DockerBuildsView>('/v1/docker/builds'), 4000);
  const v = view.data;
  if (view.loading && !v) return <Skeleton h={120} />;
  if (!v) {
    return (
      <Card title="Build history">
        <p className="fx-muted">Could not read the build history{view.error ? `: ${view.error.message}` : ''}. An older FluxVM daemon does not have it; run <code className="fx-mono">{HISTORY_COMMAND}</code> in a terminal instead.</p>
      </Card>
    );
  }
  const sum = summarizeBuilds(v.builds);
  return (
    <>
      <Card title="Build with docker" action={<Copy1 notify={notify} cmd={CONTEXT_COMMAND} />}>
        <p className="fx-muted">
          <Pill tone={v.available ? 'ok' : 'warn'}>{v.available ? 'ready' : 'cannot build here'}</Pill> {builderNote(v)}
        </p>
        <p className="fx-muted">
          Point <code className="fx-mono">docker</code> at it once with <code className="fx-mono">{CONTEXT_COMMAND}</code>, then <code className="fx-mono">{v.try}</code>. The same build without docker: <code className="fx-mono">{v.fluxctl}</code>.
        </p>
      </Card>
      <div className="fx-section">
        <Card title="Build history" action={<Copy1 notify={notify} cmd={HISTORY_COMMAND} />}>
          {v.builds.length === 0 ? (
            <EmptyState title="No builds yet" body={`Run ${v.try} against the FluxVM socket and it shows up here with its result.`} />
          ) : (
            <>
              <p className="fx-muted">
                <Pill tone="ok">{sum.built} built</Pill>{' '}
                {sum.failed ? <Pill tone="bad">{sum.failed} failed</Pill> : null}{' '}
                {sum.running ? <Pill tone="info">{sum.running} running</Pill> : null}
              </p>
              <div className="fx-table-wrap">
                <table className="fx-table">
                  <thead><tr><th>Name</th><th>Result</th><th>Takes</th><th>Image</th><th>Context</th><th>When</th><th /></tr></thead>
                  <tbody>
                    {v.builds.map((b) => (
                      <tr key={b.id}>
                        <td className="vm-name">{tagsText(b)}{b.target ? <span className="fx-muted"> · target {b.target}</span> : null}</td>
                        <td>
                          <Pill tone={buildTone(b.state)}>{buildLabel(b.state)}</Pill>
                          {b.error ? <div className="fx-muted" role="note">{errorLine(b)}</div> : null}
                        </td>
                        <td className="fx-muted">{durationText(b.duration_ms)}</td>
                        <td className="fx-mono fx-muted">{shortImage(b.image_id)}</td>
                        <td className="fx-muted">{sizeText(b.context_bytes)}</td>
                        <td className="fx-muted">{ago(new Date(b.started_unix * 1000).toISOString())}</td>
                        <td><span className="fx-row-actions"><Copy1 notify={notify} cmd={b.fluxctl} /></span></td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            </>
          )}
        </Card>
      </div>
    </>
  );
}
