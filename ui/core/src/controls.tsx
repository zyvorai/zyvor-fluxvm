import { type ReactNode, useId } from 'react';

export function Tabs<T extends string>({ value, tabs, onChange }: { value: T; tabs: { value: T; label: string }[]; onChange: (v: T) => void }) {
  return (
    <div className="fx-tabs" role="tablist">
      {tabs.map((t) => (
        <button key={t.value} role="tab" aria-selected={t.value === value} className={t.value === value ? 'is-active' : ''} onClick={() => onChange(t.value)}>
          {t.label}
        </button>
      ))}
    </div>
  );
}

export function Field({ label, hint, children }: { label: string; hint?: string; children: (id: string) => ReactNode }) {
  const id = useId();
  return (
    <div className="fx-field">
      <label htmlFor={id}>{label}</label>
      {children(id)}
      {hint ? <small>{hint}</small> : null}
    </div>
  );
}

/** Tiny dependency-free area sparkline; `values` are plotted left→right, scaled to `max` (or the data). */
export function Sparkline({ values, max, tone = 'info', height = 44 }: { values: number[]; max?: number; tone?: 'ok' | 'warn' | 'bad' | 'info'; height?: number }) {
  const w = 160;
  if (values.length < 2) return <svg className={`fx-spark fx-tone-${tone}`} viewBox={`0 0 ${w} ${height}`} aria-hidden />;
  const top = max ?? Math.max(1, ...values);
  const pts = values.map((v, i) => [(i / (values.length - 1)) * w, height - 2 - (Math.min(v, top) / top) * (height - 6)] as const);
  const line = pts.map(([x, y], i) => `${i ? 'L' : 'M'}${x.toFixed(1)},${y.toFixed(1)}`).join(' ');
  return (
    <svg className={`fx-spark fx-tone-${tone}`} viewBox={`0 0 ${w} ${height}`} preserveAspectRatio="none" aria-hidden>
      <path d={`${line} L${w},${height} L0,${height} Z`} className="fill" />
      <path d={line} className="line" />
    </svg>
  );
}

export function Meter({ value, label, tone }: { value: number; label?: string; tone?: 'ok' | 'warn' | 'bad' | 'info' }) {
  const pct = Math.max(0, Math.min(100, value));
  const t = tone ?? (pct > 90 ? 'bad' : pct > 75 ? 'warn' : 'info');
  return (
    <div className={`fx-meter fx-tone-${t}`} role="meter" aria-valuenow={Math.round(pct)} aria-valuemin={0} aria-valuemax={100} aria-label={label}>
      <span style={{ width: `${pct}%` }} />
    </div>
  );
}

export function KV({ rows }: { rows: [string, ReactNode][] }) {
  return (
    <dl className="fx-kv">
      {rows.map(([k, v]) => (
        <div key={k}>
          <dt>{k}</dt>
          <dd>{v ?? '—'}</dd>
        </div>
      ))}
    </dl>
  );
}

export const Steps = ({ steps, index }: { steps: string[]; index: number }) => (
  <ol className="fx-steps" aria-label="Progress">
    {steps.map((s, i) => (
      <li key={s} className={i === index ? 'is-now' : i < index ? 'is-done' : ''} aria-current={i === index ? 'step' : undefined}>
        <i>{i < index ? '✓' : i + 1}</i>
        {s}
      </li>
    ))}
  </ol>
);
