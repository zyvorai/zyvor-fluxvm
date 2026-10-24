// Pure helpers for a VM's screen: what the node accepts for keyboard and mouse input, so the browser can refuse first.
// Shared by the node Studio and the fleet console (their displays are the same component).

export interface InputAction {
  action: string;
  [k: string]: unknown;
}

/** The guest types printable ASCII, newline and tab (US layout); anything else would fail the whole batch. */
export const typable = (t: string): boolean => /^[\x20-\x7e\n\t]*$/.test(t);

export interface KeyLike {
  key: string;
  code: string;
  metaKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  shiftKey: boolean;
  isComposing?: boolean;
}

const NAMED: Record<string, string> = {
  Enter: 'return', Tab: 'tab', Escape: 'escape', Backspace: 'backspace', Delete: 'delete', ArrowUp: 'up', ArrowDown: 'down',
  ArrowLeft: 'left', ArrowRight: 'right', Home: 'home', End: 'end', PageUp: 'page_up', PageDown: 'page_down', ' ': 'space',
};
const MODIFIER_KEYS = ['Shift', 'Control', 'Alt', 'Meta', 'CapsLock', 'Dead', 'Unidentified', 'Process'];

/** The physical letter or digit a shortcut names, so Option+E is `e` rather than the accent it would type. */
function shortcutKey(e: KeyLike): string | null {
  if (/^Key[A-Z]$/.test(e.code)) return e.code.slice(3).toLowerCase();
  if (/^Digit[0-9]$/.test(e.code)) return e.code.slice(5);
  return e.key.length === 1 && typable(e.key) && e.key !== ' ' ? e.key.toLowerCase() : null;
}

/** What a key press means for the guest: text, a named key with modifiers, or nothing (a lone modifier, an untypable key). */
export function keyAction(e: KeyLike): InputAction | null {
  if (e.isComposing || MODIFIER_KEYS.includes(e.key)) return null;
  const modifiers = [e.metaKey && 'command', e.ctrlKey && 'control', e.altKey && 'option', e.shiftKey && 'shift'].filter(Boolean) as string[];
  const plain = !e.metaKey && !e.ctrlKey && !e.altKey;
  if (e.key.length === 1 && plain) return typable(e.key) ? { action: 'type', text: e.key } : null;
  const named = NAMED[e.key] ?? (/^F([1-9]|1[0-2])$/.test(e.key) ? e.key.toLowerCase() : null);
  if (named) return { action: 'key', key: named, modifiers: plain ? modifiers.filter((m) => m === 'shift') : modifiers };
  const key = plain ? null : shortcutKey(e);
  return key ? { action: 'key', key, modifiers } : null;
}

/** Consecutive `type` actions become one, up to 4096 characters, so a burst of keys is one request. */
export function mergeTypes(actions: InputAction[]): InputAction[] {
  const out: InputAction[] = [];
  for (const a of actions) {
    const last = out[out.length - 1];
    if (a.action === 'type' && last?.action === 'type' && String(last.text).length + String(a.text).length <= 4096) last.text = String(last.text) + String(a.text);
    else out.push({ ...a });
  }
  return out;
}

/** Where a pointer falls in the screenshot's own pixels; the image is letterboxed (object-fit: contain). Null outside it. */
export function screenPoint(box: { left: number; top: number; width: number; height: number }, natural: { w: number; h: number }, clientX: number, clientY: number): { x: number; y: number; screen_width: number } | null {
  const nw = natural.w || 1;
  const nh = natural.h || 1;
  const scale = Math.min(box.width / nw, box.height / nh);
  if (!(scale > 0)) return null;
  const x = (clientX - box.left - (box.width - nw * scale) / 2) / scale;
  const y = (clientY - box.top - (box.height - nh * scale) / 2) / scale;
  if (x < 0 || y < 0 || x > nw || y > nh) return null;
  return { x: Math.round(x), y: Math.round(y), screen_width: nw };
}

/** Wheel deltas become 0–5 notches per event. */
export const scrollNotches = (d: number): number => Math.sign(d) * Math.min(5, Math.ceil(Math.abs(d) / 40));
