import { apiBase } from '@fluxvm/ui-core';

/** `ws(s)://host/<prefix><path>` for a same-origin path such as `/console/ws/<ticket>?cols=80&rows=24`. */
export function wsUrl(path: string, loc: Pick<Location, 'protocol' | 'host'> = location, base = apiBase()): string {
  return `${loc.protocol === 'https:' ? 'wss:' : 'ws:'}//${loc.host}${base}${path}`;
}

export type ResizeFormat = 'none' | 'flat' | 'nested';

/** VM guest shell: `{cols,rows}`. Agent-session terminal: `{resize:{cols,rows}}`. */
export const resizeMessage = (cols: number, rows: number, format: Exclude<ResizeFormat, 'none'> = 'flat'): string => JSON.stringify(format === 'nested' ? { resize: { cols, rows } } : { cols, rows });

/** Reads CSS tokens so the terminal follows the active theme. */
export function termTheme(): Record<string, string> {
  const cs = getComputedStyle(document.documentElement);
  const v = (n: string, d: string) => cs.getPropertyValue(n).trim() || d;
  return { background: v('--terminal-bg', '#000'), foreground: v('--terminal-text', '#f5f5f7'), cursor: v('--apple-link', '#2997ff'), selectionBackground: 'rgba(41,151,255,.35)' };
}
