import { apiBase } from './api';

/** `ws(s)://host/<prefix><path>` for a same-origin path such as `/console/ws/<ticket>?cols=80&rows=24`. */
export function wsUrl(path: string, loc: Pick<Location, 'protocol' | 'host'> = location, base = apiBase()): string {
  return `${loc.protocol === 'https:' ? 'wss:' : 'ws:'}//${loc.host}${base}${path}`;
}
