// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { useState } from 'react';
import { Button, Card, Field, Hero, Sheet } from '@fluxvm/ui-core';
import type { Ctx } from '../App';
import { RECIPES, type Recipe, runLine, showLine } from '../lib/recipes';

/** Central does not run commands on machines, so Run shows the exact line for the machine you want it on, and Copy puts it on the clipboard. */
export function RecipesPage({ notify }: Ctx) {
  const [open, setOpen] = useState<Recipe | null>(null);
  const [values, setValues] = useState<Record<string, string>>({});
  const copy = async (text: string) => {
    try { await navigator.clipboard.writeText(text); notify('Copied'); } catch { notify('Could not copy: select the line and copy it', 'bad'); }
  };
  return (
    <>
      <Hero eyebrow="Recipes" title="Run something useful" accent="with one command." lede="Pick a recipe, fill in the blanks, and run the line on the machine that should host it. Each recipe prints where to open it, where its credentials are, a backup schedule and how to undo it." />
      <div className="fx-grid cols-2">
        {RECIPES.map((r) => (
          <Card key={r.name} title={r.title}>
            <p className="fx-muted">{r.summary}</p>
            <div className="fx-actions">
              <Button variant="primary" onClick={() => { setValues({}); setOpen(r); }}>Run</Button>
              <Button onClick={() => copy(showLine(r))}>Copy as fluxctl</Button>
            </div>
          </Card>
        ))}
      </div>
      <Sheet open={!!open} title={open?.title ?? ''} onClose={() => setOpen(null)}>
        {open ? (
          <>
            {open.params.map((p) => (
              <Field key={p.key} label={p.key} hint={p.help}>{(id) => <input id={id} className="fx-input" value={values[p.key] ?? p.value} onChange={(e) => setValues({ ...values, [p.key]: e.target.value })} />}</Field>
            ))}
            <p>On the machine that should run it:</p>
            <pre className="fx-mono">{runLine(open, values)}</pre>
            <p className="fx-muted">Add <b>--dry-run</b> to see the steps first. Credentials it generates are stored in a private file on that machine and are never printed here.</p>
            <div className="fx-actions"><Button variant="primary" onClick={() => copy(runLine(open, values))}>Copy as fluxctl</Button><Button onClick={() => setOpen(null)}>Close</Button></div>
          </>
        ) : null}
      </Sheet>
    </>
  );
}
