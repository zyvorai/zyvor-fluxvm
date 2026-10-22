import { type ButtonHTMLAttributes, type ReactNode, useEffect, useRef, useState } from 'react';
import { Moon, Sun, X } from 'lucide-react';
import { useCountUp, useReducedMotion } from './hooks';
import { currentTheme, toggleTheme } from './theme';

type Tone = 'ok' | 'warn' | 'bad' | 'info' | 'idle';

export function Button({
  variant = 'secondary',
  size = 'md',
  busy,
  className = '',
  children,
  ...rest
}: ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: 'primary' | 'secondary' | 'ghost' | 'danger';
  size?: 'sm' | 'md' | 'lg';
  busy?: boolean;
}) {
  return (
    <button
      {...rest}
      disabled={rest.disabled || busy}
      aria-busy={busy || undefined}
      className={`fx-btn fx-btn-${variant} fx-btn-${size} ${busy ? 'is-busy' : ''} ${className}`}
    >
      {busy ? <span className="fx-spin" aria-hidden /> : null}
      {children}
    </button>
  );
}

export function Pill({ tone = 'idle', dot = true, raw = false, children }: { tone?: Tone; dot?: boolean; raw?: boolean; children: ReactNode }) {
  return (
    <span className={`fx-pill fx-tone-${tone}${raw ? ' is-raw' : ''}`}>
      {dot ? <i aria-hidden /> : null}
      {children}
    </span>
  );
}

export function Card({
  title,
  action,
  children,
  className = '',
}: {
  title?: ReactNode;
  action?: ReactNode;
  children: ReactNode;
  className?: string;
}) {
  return (
    <section className={`fx-card ${className}`}>
      {title || action ? (
        <header className="fx-card-head">
          <h2>{title}</h2>
          {action}
        </header>
      ) : null}
      {children}
    </section>
  );
}

export function Stat({ label, value, hint, tone }: { label: string; value: number | string; hint?: string; tone?: Tone }) {
  const n = typeof value === 'number' ? value : NaN;
  const counted = useCountUp(Number.isNaN(n) ? 0 : n);
  return (
    <div className={`fx-stat ${tone ? `fx-tone-${tone}` : ''}`}>
      <div className="fx-stat-num">{Number.isNaN(n) ? value : counted}</div>
      <div className="fx-stat-label">{label}</div>
      {hint ? <div className="fx-stat-hint">{hint}</div> : null}
    </div>
  );
}

export const Skeleton = ({ h = 16, w = '100%' }: { h?: number; w?: number | string }) => (
  <span className="fx-skel" style={{ height: h, width: w }} aria-hidden />
);

export function Segmented<T extends string>({
  value,
  options,
  onChange,
  label,
}: {
  value: T;
  options: { value: T; label: string }[];
  onChange: (v: T) => void;
  label: string;
}) {
  const idx = Math.max(0, options.findIndex((o) => o.value === value));
  return (
    <div className="fx-seg" role="radiogroup" aria-label={label} style={{ ['--n' as string]: options.length, ['--i' as string]: idx }}>
      <span className="fx-seg-thumb" aria-hidden />
      {options.map((o) => (
        <button key={o.value} role="radio" aria-checked={o.value === value} onClick={() => onChange(o.value)}>
          {o.label}
        </button>
      ))}
    </div>
  );
}

export function Sheet({ open, title, onClose, children }: { open: boolean; title: string; onClose: () => void; children: ReactNode }) {
  useEffect(() => {
    if (!open) return;
    const f = (e: KeyboardEvent) => e.key === 'Escape' && onClose();
    window.addEventListener('keydown', f);
    return () => window.removeEventListener('keydown', f);
  }, [open, onClose]);
  if (!open) return null;
  return (
    <div className="fx-sheet-wrap" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <aside className="fx-sheet" role="dialog" aria-modal="true" aria-label={title}>
        <header>
          <h2>{title}</h2>
          <button className="fx-icon-btn" onClick={onClose} aria-label="Close">
            <X size={18} />
          </button>
        </header>
        <div className="fx-sheet-body">{children}</div>
      </aside>
    </div>
  );
}

/** Fades/rises its children in when they scroll into view (skipped under reduced motion). */
export function Reveal({ children, delay = 0 }: { children: ReactNode; delay?: number }) {
  const ref = useRef<HTMLDivElement>(null);
  const reduced = useReducedMotion();
  const [shown, setShown] = useState(reduced);
  useEffect(() => {
    if (reduced || shown) {
      setShown(true);
      return;
    }
    const el = ref.current;
    if (!el || typeof IntersectionObserver === 'undefined') {
      setShown(true);
      return;
    }
    const io = new IntersectionObserver(
      ([e]) => {
        if (e.isIntersecting) {
          setShown(true);
          io.disconnect();
        }
      },
      { threshold: 0.12 },
    );
    io.observe(el);
    return () => io.disconnect();
  }, [reduced, shown]);
  return (
    <div ref={ref} className={`fx-reveal ${shown ? 'is-in' : ''}`} style={{ transitionDelay: `${delay}ms` }}>
      {children}
    </div>
  );
}

export function ThemeToggle() {
  const [t, setT] = useState(currentTheme());
  return (
    <button className="fx-icon-btn" aria-label={`Switch to ${t === 'dark' ? 'light' : 'dark'} theme`} onClick={() => setT(toggleTheme())}>
      {t === 'dark' ? <Sun size={17} /> : <Moon size={17} />}
    </button>
  );
}

export function Hero({ eyebrow, title, accent, lede, children }: { eyebrow?: string; title: string; accent?: string; lede?: string; children?: ReactNode }) {
  return (
    <header className="fx-hero">
      <div className="fx-hero-glow" aria-hidden />
      {eyebrow ? <div className="fx-eyebrow">{eyebrow}</div> : null}
      <h1>
        {title} {accent ? <span className="fx-gradient">{accent}</span> : null}
      </h1>
      {lede ? <p className="fx-lede">{lede}</p> : null}
      {children ? <div className="fx-hero-actions">{children}</div> : null}
    </header>
  );
}

export function EmptyState({ title, body, action }: { title: string; body: string; action?: ReactNode }) {
  return (
    <div className="fx-empty">
      <div className="fx-empty-orb" aria-hidden />
      <h3>{title}</h3>
      <p>{body}</p>
      {action}
    </div>
  );
}

export type ToastItem = { id: number; text: string; tone: 'ok' | 'bad' };
export function Toasts({ items }: { items: ToastItem[] }) {
  return (
    <div className="fx-toasts" aria-live="polite">
      {items.map((t) => (
        <div key={t.id} className={`fx-toast fx-tone-${t.tone}`}>
          {t.text}
        </div>
      ))}
    </div>
  );
}
