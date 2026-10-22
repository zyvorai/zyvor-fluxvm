import { type ReactNode, useEffect, useMemo, useRef, useState } from 'react';
import { CornerDownLeft } from 'lucide-react';

export interface Command {
  id: string;
  label: string;
  hint?: string;
  group: string;
  icon?: ReactNode;
  run: () => void;
}

export const filterCommands = (cmds: Command[], q: string): Command[] => {
  const needle = q.trim().toLowerCase();
  if (!needle) return cmds;
  return cmds.filter((c) => `${c.label} ${c.group} ${c.hint ?? ''}`.toLowerCase().includes(needle));
};

/** ⌘K / Ctrl-K palette: navigation, VM jump and actions in one fuzzy-ish list. */
export function CommandPalette({ open, onClose, commands }: { open: boolean; onClose: () => void; commands: Command[] }) {
  const [q, setQ] = useState('');
  const [sel, setSel] = useState(0);
  const input = useRef<HTMLInputElement>(null);
  const list = useMemo(() => filterCommands(commands, q).slice(0, 40), [commands, q]);
  useEffect(() => {
    if (open) {
      setQ('');
      setSel(0);
      setTimeout(() => input.current?.focus(), 0);
    }
  }, [open]);
  useEffect(() => setSel(0), [q]);
  if (!open) return null;
  const run = (c?: Command) => {
    if (!c) return;
    onClose();
    c.run();
  };
  return (
    <div className="fx-palette-wrap" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="fx-palette" role="dialog" aria-modal="true" aria-label="Command palette">
        <input
          ref={input}
          value={q}
          placeholder="Search machines, pages and actions…"
          onChange={(e) => setQ(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'ArrowDown') (e.preventDefault(), setSel((s) => Math.min(s + 1, list.length - 1)));
            else if (e.key === 'ArrowUp') (e.preventDefault(), setSel((s) => Math.max(s - 1, 0)));
            else if (e.key === 'Enter') run(list[sel]);
            else if (e.key === 'Escape') onClose();
          }}
        />
        <ul>
          {list.length === 0 ? <li className="fx-palette-none">Nothing matches “{q}”.</li> : null}
          {list.map((c, i) => (
            <li key={c.id} className={i === sel ? 'is-sel' : ''} onMouseEnter={() => setSel(i)} onClick={() => run(c)}>
              <span className="fx-palette-ico">{c.icon}</span>
              <span className="fx-palette-label">{c.label}</span>
              <small>{c.hint ?? c.group}</small>
              {i === sel ? <CornerDownLeft size={13} /> : null}
            </li>
          ))}
        </ul>
      </div>
    </div>
  );
}
