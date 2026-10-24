import { useEffect } from 'react';
import { X } from 'lucide-react';
import { type DockerContainer, ageSince } from '../lib/docker';
import { LogsPane } from './PodWindow';

/** A container's log in the same Terminal.app-style window as a pod's. Read-only: the engine's logs, nothing else. */
export function ContainerWindow({ container, onClose }: { container: DockerContainer; onClose: () => void }) {
  useEffect(() => {
    const f = (e: KeyboardEvent) => e.key === 'Escape' && onClose();
    window.addEventListener('keydown', f);
    return () => window.removeEventListener('keydown', f);
  }, [onClose]);
  return (
    <div className="kx-wrap" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <section className="kx-window" role="dialog" aria-modal="true" aria-label={`${container.name} logs`}>
        <header className="kx-titlebar">
          <span className="kx-dots">
            <button className="kx-dot kx-dot-close" aria-label="Close" onClick={onClose}><X size={8} strokeWidth={3} /></button>
            <span className="kx-dot kx-dot-min" aria-hidden />
            <span className="kx-dot kx-dot-max" aria-hidden />
          </span>
          <span className="kx-title">
            <b>{container.name}</b>
            <span className="kx-sub"> — {container.image} — {container.status}</span>
          </span>
        </header>
        <LogsPane base={`/v1/docker/containers/${encodeURIComponent(container.id)}`} logPath="logs" container="" canPrevious={false} name={container.name} up={`created ${ageSince(container.created, Date.now())} ago`} />
      </section>
    </div>
  );
}
