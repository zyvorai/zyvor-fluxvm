import type { ReactNode } from 'react';

const fmt = (v: unknown): ReactNode => {
  if (v == null) return '—';
  if (typeof v === 'number') return Number.isInteger(v) ? v.toLocaleString() : v.toFixed(2);
  if (typeof v === 'boolean') return v ? 'yes' : 'no';
  if (typeof v === 'object') return <code className="fx-mono">{JSON.stringify(v).slice(0, 80)}</code>;
  return String(v);
};

/** First array found in a response: either the value itself or under a conventional key. */
export function rowsOf(data: unknown, ...keys: string[]): Record<string, unknown>[] {
  if (Array.isArray(data)) return data as Record<string, unknown>[];
  if (data && typeof data === 'object') {
    const o = data as Record<string, unknown>;
    for (const k of [...keys, 'items', 'flows', 'drops', 'entries']) if (Array.isArray(o[k])) return o[k] as Record<string, unknown>[];
  }
  return [];
}

export const columnsOf = (rows: Record<string, unknown>[], max = 6): string[] => {
  const seen = new Map<string, number>();
  for (const r of rows.slice(0, 20)) for (const k of Object.keys(r)) if (typeof r[k] !== 'object' || r[k] === null) seen.set(k, (seen.get(k) ?? 0) + 1);
  return [...seen].sort((a, b) => b[1] - a[1]).slice(0, max).map(([k]) => k);
};

export function GenericTable({ rows, empty }: { rows: Record<string, unknown>[]; empty: string }) {
  if (!rows.length) return <p className="fx-muted">{empty}</p>;
  const cols = columnsOf(rows);
  return (
    <div className="fx-table-wrap">
      <table className="fx-table">
        <thead><tr>{cols.map((c) => <th key={c}>{c.replace(/_/g, ' ')}</th>)}</tr></thead>
        <tbody>{rows.slice(0, 100).map((r, i) => <tr key={i}>{cols.map((c) => <td key={c} className={typeof r[c] === 'number' || /ip|addr|dst|src/.test(c) ? 'fx-mono' : ''}>{fmt(r[c])}</td>)}</tr>)}</tbody>
      </table>
    </div>
  );
}
