// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { useState } from 'react';
import { Copy } from 'lucide-react';
import { Button, Card, EmptyState, Hero, Skeleton, api, usePoll } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { cheatsheetPath, filterCheatsheet, type Cheatsheet } from '../lib/templates';

/** The commands people reach for first, grouped, with a search box and a copy button for each. The same as fluxctl cheatsheet. */
export function CheatsheetPage({ notify }: { notify: Notify }) {
  const poll = usePoll(() => api.get<Cheatsheet>(cheatsheetPath), 300000);
  const [q, setQ] = useState('');
  const copy = (cmd: string) => navigator.clipboard.writeText(cmd).then(() => notify('Copied: ' + cmd), () => notify('Copy blocked by the browser', 'bad'));
  const groups = filterCheatsheet(poll.data?.groups ?? [], q);
  return (
    <>
      <Hero eyebrow="Cheatsheet" title="The commands" accent="you will want." lede="Grouped by what you are trying to do. Copy one and paste it into a terminal. The same as fluxctl cheatsheet." />
      {!poll.data ? (
        poll.loading ? <Skeleton /> : <EmptyState title="Could not load the cheatsheet" body="This daemon may be older than this page. In a terminal: fluxctl cheatsheet" />
      ) : (
        <>
          <p><input className="fx-input" value={q} onChange={(e) => setQ(e.target.value)} placeholder="Search commands" aria-label="Search commands" /></p>
          {groups.length === 0 ? <EmptyState title="Nothing matches" body="Clear the search to see every command." /> : groups.map((g) => (
            <div className="fx-section" key={g.group}>
              <Card title={g.group}>
                <ul className="fx-feed">
                  {g.commands.map((c) => (
                    <li key={c.command}>
                      <span><code className="fx-mono">{c.command}</code><div className="fx-muted">{c.does}</div></span>
                      <Button size="sm" variant="ghost" aria-label={`Copy ${c.command}`} title={c.command} onClick={() => void copy(c.command)}><Copy size={14} /></Button>
                    </li>
                  ))}
                </ul>
              </Card>
            </div>
          ))}
        </>
      )}
    </>
  );
}
