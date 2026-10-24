export { wsUrl } from '@fluxvm/ui-core';

export type ResizeFormat = 'none' | 'flat' | 'nested';

/** VM guest shell: `{cols,rows}`. Agent-session terminal: `{resize:{cols,rows}}`. */
export const resizeMessage = (cols: number, rows: number, format: Exclude<ResizeFormat, 'none'> = 'flat'): string => JSON.stringify(format === 'nested' ? { resize: { cols, rows } } : { cols, rows });

/** Reads CSS tokens so the terminal follows the active theme. */
export function termTheme(): Record<string, string> {
  const cs = getComputedStyle(document.documentElement);
  const v = (n: string, d: string) => cs.getPropertyValue(n).trim() || d;
  return { background: v('--terminal-bg', '#000'), foreground: v('--terminal-text', '#f5f5f7'), cursor: v('--apple-link', '#2997ff'), selectionBackground: 'rgba(41,151,255,.35)' };
}
