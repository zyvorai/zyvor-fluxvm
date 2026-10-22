import { type ReactNode, useEffect, useRef, useState } from 'react';
import { ChevronRight, Command, Menu, Search } from 'lucide-react';
import { ThemeToggle } from './components';

export interface NavItem {
  to: string;
  label: string;
  desc?: string;
  icon?: ReactNode;
}
export interface NavGroup {
  label: string;
  items: NavItem[];
}

/** Glass top bar with a mega-menu per group, a ⌘K trigger, theme toggle and an identity slot. */
export function GlobalNav({
  brand,
  groups,
  active,
  onPalette,
  right,
}: {
  brand: ReactNode;
  groups: NavGroup[];
  active: string;
  onPalette: () => void;
  right?: ReactNode;
}) {
  const [open, setOpen] = useState<string | null>(null);
  const [mobile, setMobile] = useState(false);
  const ref = useRef<HTMLElement>(null);
  useEffect(() => {
    const away = (e: MouseEvent) => ref.current && !ref.current.contains(e.target as Node) && setOpen(null);
    const esc = (e: KeyboardEvent) => e.key === 'Escape' && (setOpen(null), setMobile(false));
    document.addEventListener('mousedown', away);
    document.addEventListener('keydown', esc);
    return () => {
      document.removeEventListener('mousedown', away);
      document.removeEventListener('keydown', esc);
    };
  }, []);
  const isActive = (g: NavGroup) => g.items.some((i) => active === i.to || active.startsWith(i.to + '/'));
  const go = () => (setOpen(null), setMobile(false));
  return (
    <nav className="fx-nav" ref={ref} aria-label="Primary">
      <div className="fx-nav-inner">
        <a className="fx-brand" href="#/" onClick={go}>
          {brand}
        </a>
        <button className="fx-icon-btn fx-nav-burger" aria-label="Menu" aria-expanded={mobile} onClick={() => setMobile((m) => !m)}>
          <Menu size={18} />
        </button>
        <div className={`fx-nav-links ${mobile ? 'is-open' : ''}`}>
          {groups.map((g) =>
            g.items.length === 1 ? (
              <a key={g.label} href={'#' + g.items[0].to} className={isActive(g) ? 'is-active' : ''} onClick={go}>
                {g.label}
              </a>
            ) : (
              <div key={g.label} className="fx-nav-group" onMouseEnter={() => setOpen(g.label)} onMouseLeave={() => setOpen(null)}>
                <button className={isActive(g) ? 'is-active' : ''} aria-expanded={open === g.label} onClick={() => setOpen(open === g.label ? null : g.label)}>
                  {g.label}
                </button>
                <div className={`fx-mega ${open === g.label ? 'is-open' : ''}`}>
                  {g.items.map((i) => (
                    <a key={i.to} href={'#' + i.to} onClick={go} className={active === i.to || active.startsWith(i.to + '/') ? 'is-active' : ''}>
                      <span className="fx-mega-ico">{i.icon}</span>
                      <span>
                        <b>{i.label}</b>
                        {i.desc ? <small>{i.desc}</small> : null}
                      </span>
                      <ChevronRight size={14} className="fx-mega-chev" />
                    </a>
                  ))}
                </div>
              </div>
            ),
          )}
        </div>
        <div className="fx-nav-right">
          <button className="fx-palette-trigger" onClick={onPalette} aria-label="Search and commands">
            <Search size={14} />
            <span>Search</span>
            <kbd>
              <Command size={11} />K
            </kbd>
          </button>
          <ThemeToggle />
          {right}
        </div>
      </div>
    </nav>
  );
}
