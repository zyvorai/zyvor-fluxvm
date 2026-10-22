import { useCallback, useState } from 'react';
import type { Notify } from '../App';

/** One busy key at a time, errors to a toast. Returns true when the action succeeded. */
export function useAct(notify: Notify) {
  const [busy, setBusy] = useState('');
  const run = useCallback(
    async (key: string, fn: () => Promise<unknown>, ok?: string, after?: () => Promise<unknown> | void): Promise<boolean> => {
      setBusy(key);
      try {
        await fn();
        if (ok) notify(ok);
        await after?.();
        return true;
      } catch (e) {
        notify((e as Error).message, 'bad');
        return false;
      } finally {
        setBusy('');
      }
    },
    [notify],
  );
  return { busy, run };
}
