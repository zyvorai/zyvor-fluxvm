// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { useState } from 'react';
import { Copy, Play } from 'lucide-react';
import { Button, Card, EmptyState, Hero, Pill, Skeleton, api, usePoll } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { useAct } from '../lib/act';
import { filterTemplates, kindWord, needsWord, runPath, templatesPath, type TemplateGallery } from '../lib/templates';

/** One-command stacks built into fluxctl: pick one, start it, copy the same thing as a terminal command. */
export function TemplatesPage({ notify }: { notify: Notify }) {
  const poll = usePoll(() => api.get<TemplateGallery>(templatesPath), 60000);
  const { busy, run } = useAct(notify);
  const [q, setQ] = useState('');
  const copy = (cmd: string) => navigator.clipboard.writeText(cmd).then(() => notify('Copied: ' + cmd), () => notify('Copy blocked by the browser', 'bad'));
  const shown = filterTemplates(poll.data?.templates ?? [], q);
  return (
    <>
      <Hero eyebrow="Templates" title="A running stack," accent="in one command." lede="Postgres, Redis, a mail catcher, nginx on k3s. Each is a plain file you can read first. The same as fluxctl templates run." />
      {!poll.data ? (
        poll.loading ? <Skeleton /> : <EmptyState title="Could not load the templates" body="This daemon may be older than this page. In a terminal: fluxctl templates ls" />
      ) : (
        <>
          <p><input className="fx-input" value={q} onChange={(e) => setQ(e.target.value)} placeholder="Search templates" aria-label="Search templates" /></p>
          {shown.length === 0 ? <EmptyState title="No template matches" body="Clear the search to see them all." /> : (
            <div className="fx-grid">
              {shown.map((t) => (
                <Card key={t.name} title={t.title} action={<Pill tone="idle">{kindWord(t.kind)}</Pill>}>
                  <p>{t.summary}</p>
                  <p className="fx-muted">{needsWord(t.kind)}</p>
                  <p className="fx-muted">After it starts: {t.after}</p>
                  <p>
                    <Button size="sm" busy={busy === t.name} onClick={() => void run(t.name, () => api.post(runPath(t.name), {}), `${t.title} started`)}><Play size={14} /> Start</Button>{' '}
                    <Button size="sm" variant="ghost" aria-label={`Copy ${t.run}`} title={t.run} onClick={() => void copy(t.run)}><Copy size={14} /> Copy as fluxctl</Button>
                  </p>
                </Card>
              ))}
            </div>
          )}
          <p className="fx-muted">Start needs an admin token. It runs <code className="fx-mono">fluxctl templates run NAME</code> on the machine that runs the daemon; the file is written to ~/.local/share/fluxvm/templates/NAME/ so you can read or edit it.</p>
        </>
      )}
    </>
  );
}
