// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { useCallback, useEffect, useState } from 'react';
import { Download } from 'lucide-react';
import { Button, Card, EmptyState, Hero, Pill, Skeleton, api } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { CopyCmd } from './Images';
import { EMPTY_FILTERS, SINCE_CHOICES, actionWords, auditPath, emptyWords, lsCommand, resultTone, resultWord, toJsonl, type AuditFilters, type AuditPage, type AuditRecord } from '../lib/audit';

const PAGE = 50;

/** The audit log: every change, who made it, when, and whether it worked. Filters mirror `fluxctl audit ls`. */
export function AuditPage({ notify }: { notify: Notify }) {
  const [filters, setFilters] = useState<AuditFilters>(EMPTY_FILTERS);
  const [items, setItems] = useState<AuditRecord[]>([]);
  const [meta, setMeta] = useState<{ total: number; next?: number; notice?: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const set = (k: keyof AuditFilters, v: string) => setFilters((f) => ({ ...f, [k]: v }));

  const load = useCallback(async (f: AuditFilters, offset: number) => {
    setLoading(true);
    try {
      const page = await api.get<AuditPage>(auditPath(f, { limit: PAGE, offset }));
      setItems((prev) => (offset === 0 ? page.items : [...prev, ...page.items]));
      setMeta({ total: page.total, next: page.next_offset, notice: page.notice });
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setLoading(false);
    }
  }, []);

  // Reload from the top whenever a filter changes (text filters wait a moment so typing is not a request per key).
  useEffect(() => {
    const t = setTimeout(() => void load(filters, 0), 250);
    return () => clearTimeout(t);
  }, [filters, load]);

  const exportAll = async () => {
    try {
      const all: AuditRecord[] = [];
      let offset = 0;
      for (;;) {
        const page = await api.get<AuditPage>(auditPath(filters, { limit: 1000, offset }));
        all.push(...page.items);
        if (page.next_offset === undefined) break;
        offset = page.next_offset;
      }
      const url = URL.createObjectURL(new Blob([toJsonl(all)], { type: 'application/x-ndjson' }));
      const a = document.createElement('a');
      a.href = url;
      a.download = 'fluxvm-audit.jsonl';
      a.click();
      URL.revokeObjectURL(url);
      notify(`Exported ${all.length} records`);
    } catch (e) {
      notify(`Could not export: ${(e as Error).message}`, 'bad');
    }
  };

  const forbidden = !!error && /admin|403|forbidden/i.test(error);
  return (
    <>
      <Hero eyebrow="Audit" title="Who changed what," accent="and when." lede="Every change made through fluxctl or the API: the action, what it touched, who did it and whether it worked. Secrets are never recorded. Reads are not listed." />
      <Card title="Filters" action={<><CopyCmd notify={notify} cmd={lsCommand(filters)} /> <Button size="sm" variant="ghost" onClick={() => void exportAll()}><Download size={14} /> Export</Button></>}>
        <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
          <select className="fx-input" style={{ width: 'auto' }} aria-label="When" value={filters.since} onChange={(e) => set('since', e.target.value)}>
            {SINCE_CHOICES.map((s) => <option key={s.value} value={s.value}>{s.label}</option>)}
          </select>
          <select className="fx-input" style={{ width: 'auto' }} aria-label="Result" value={filters.result} onChange={(e) => set('result', e.target.value)}>
            <option value="">Any result</option>
            <option value="ok">Worked</option>
            <option value="failed">Failed</option>
            <option value="denied">Refused</option>
          </select>
          <input className="fx-input" style={{ width: 160 }} aria-label="Action" placeholder="Action, e.g. mac." value={filters.action} onChange={(e) => set('action', e.target.value)} />
          <input className="fx-input" style={{ width: 160 }} aria-label="Target" placeholder="Name of the thing" value={filters.target} onChange={(e) => set('target', e.target.value)} />
          <input className="fx-input" style={{ width: 140 }} aria-label="Who" placeholder="Who" value={filters.actor} onChange={(e) => set('actor', e.target.value)} />
          <Button size="sm" variant="ghost" onClick={() => setFilters(EMPTY_FILTERS)}>Clear</Button>
        </div>
      </Card>
      {meta?.notice ? <Card title="Older records were removed"><p className="fx-muted">{meta.notice}</p></Card> : null}
      <Card title={meta ? `${meta.total} ${meta.total === 1 ? 'record' : 'records'}` : 'Records'}>
        {error ? (
          <EmptyState title={forbidden ? 'Admin token needed' : 'Could not read the audit log'} body={forbidden ? 'Reading the audit log needs an admin token. Sign in with one, or run the command below on the machine.' : `${error}. Run the command below on the machine instead.`} />
        ) : loading && items.length === 0 ? (
          <Skeleton h={140} />
        ) : items.length === 0 ? (
          <EmptyState title="Nothing here" body={emptyWords(filters, meta?.total ?? 0)} />
        ) : (
          <>
            <div className="fx-table-wrap"><table className="fx-table">
              <thead><tr><th>When</th><th>What happened</th><th>Who</th><th>Result</th><th>From</th></tr></thead>
              <tbody>
                {items.map((r, i) => (
                  <tr key={`${r.ts}-${i}`}>
                    <td title={r.ts}>{new Date(r.ts).toLocaleString()}</td>
                    <td><b>{actionWords(r.action, r.target)}</b><div className="fx-muted"><code className="fx-mono">{r.action}</code>{r.detail?.error ? <> · {r.detail.error}</> : null}</div></td>
                    <td>{r.actor}</td>
                    <td><Pill tone={resultTone(r.result)}>{resultWord(r.result)}</Pill></td>
                    <td className="fx-muted">{r.source === 'api' ? 'API / Studio' : 'fluxctl'}</td>
                  </tr>
                ))}
              </tbody>
            </table></div>
            {meta?.next !== undefined ? <p><Button size="sm" busy={loading} onClick={() => void load(filters, meta.next as number)}>Show older</Button></p> : null}
          </>
        )}
      </Card>
    </>
  );
}
