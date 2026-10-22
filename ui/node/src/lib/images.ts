/** Catalog and template names share one rule on the daemon: 1-63 of [A-Za-z0-9._-], no leading - or . */
export function nameProblem(name: string): string {
  if (!name) return 'Give it a name.';
  if (name.length > 63) return 'Use at most 63 characters.';
  if (/^[-.]/.test(name)) return 'Cannot start with - or .';
  if (!/^[A-Za-z0-9._-]+$/.test(name)) return 'Use letters, digits, dot, dash and underscore.';
  return '';
}

export const isSource = (s: string): boolean => /^https?:\/\/\S+$/.test(s) || s.startsWith('/');

/** A pool is only useful with a size the daemon will accept (the resize route rejects 0). */
export function sizeProblem(raw: string): string {
  const n = Number(raw);
  if (!Number.isInteger(n) || n < 1) return 'Whole number, at least 1.';
  if (n > 256) return 'At most 256.';
  return '';
}

export function fluxctlFor(action: string, ...args: string[]): string {
  const q = (a: string) => (/^[\w./:@=,-]+$/.test(a) ? a : `'${a.replace(/'/g, `'\\''`)}'`);
  return ['fluxctl', ...action.split(' '), ...args.map(q)].join(' ');
}
