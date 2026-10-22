import { useCallback, useEffect, useRef, useState } from 'react';

export function useReducedMotion(): boolean {
  const q = '(prefers-reduced-motion: reduce)';
  const [r, setR] = useState(() => (typeof matchMedia === 'function' ? matchMedia(q).matches : false));
  useEffect(() => {
    if (typeof matchMedia !== 'function') return;
    const m = matchMedia(q);
    const f = () => setR(m.matches);
    m.addEventListener('change', f);
    return () => m.removeEventListener('change', f);
  }, []);
  return r;
}

/** Animates 0 → target (cubic ease-out); jumps straight there under reduced motion. */
export function useCountUp(target: number, ms = 700): number {
  const reduced = useReducedMotion();
  const [v, setV] = useState(reduced ? target : 0);
  const from = useRef(0);
  useEffect(() => {
    if (reduced) {
      setV(target);
      return;
    }
    const start = performance.now();
    const a = from.current;
    let raf = 0;
    const tick = (now: number) => {
      const t = Math.min(1, (now - start) / ms);
      const val = a + (target - a) * (1 - Math.pow(1 - t, 3));
      setV(val);
      from.current = val;
      if (t < 1) raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [target, ms, reduced]);
  return Math.round(v);
}

/** Polls `fn` (and once immediately). `refresh()` forces a reload. */
export function usePoll<T>(fn: () => Promise<T>, ms: number, deps: unknown[] = []) {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<Error | null>(null);
  const [loading, setLoading] = useState(true);
  const fnRef = useRef(fn);
  fnRef.current = fn;
  const run = useCallback(async () => {
    try {
      setData(await fnRef.current());
      setError(null);
    } catch (e) {
      setError(e as Error);
    } finally {
      setLoading(false);
    }
  }, []);
  useEffect(() => {
    run();
    const id = setInterval(run, ms);
    return () => clearInterval(id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ms, run, ...deps]);
  return { data, error, loading, refresh: run };
}
