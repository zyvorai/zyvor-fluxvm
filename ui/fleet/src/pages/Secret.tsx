import { Button, Sheet } from '@fluxvm/ui-core';
import type { Notify } from '../App';

/** A token or secret that exists only on this screen: copy it now, it is never shown again. */
export function SecretSheet({ title, intro, secret, notify, onClose }: { title: string; intro: string; secret: string | null; notify: Notify; onClose: () => void }) {
  return (
    <Sheet open={secret !== null} title={title} onClose={onClose}>
      <p>{intro}</p>
      <pre className="fx-code" style={{ userSelect: 'all' }}>{secret}</pre>
      <div className="fx-actions">
        <Button variant="primary" onClick={() => navigator.clipboard.writeText(secret ?? '').then(() => notify('Copied'), () => notify('Copy blocked by the browser: select it and copy by hand', 'bad'))}>Copy</Button>
        <Button onClick={onClose}>Done</Button>
      </div>
    </Sheet>
  );
}
