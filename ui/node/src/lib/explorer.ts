export interface Operation {
  method: string;
  path: string;
  summary: string;
  tag: string;
}

const METHODS = ['get', 'post', 'put', 'patch', 'delete'];

/** Operations of an OpenAPI document, sorted by path then method. Tolerates a missing or partial document. */
export function operationsOf(doc: unknown): Operation[] {
  const paths = (doc as { paths?: Record<string, Record<string, { summary?: string; description?: string; tags?: string[] }>> } | null)?.paths;
  if (!paths || typeof paths !== 'object') return [];
  const out: Operation[] = [];
  for (const [path, item] of Object.entries(paths)) {
    for (const m of METHODS) {
      const op = item?.[m];
      if (!op) continue;
      out.push({ method: m.toUpperCase(), path, summary: op.summary ?? (op.description ?? '').split('\n')[0] ?? '', tag: op.tags?.[0] ?? path.split('/')[2] ?? '' });
    }
  }
  return out.sort((a, b) => a.path.localeCompare(b.path) || METHODS.indexOf(a.method.toLowerCase()) - METHODS.indexOf(b.method.toLowerCase()));
}

export const filterOps = (ops: Operation[], q: string): Operation[] => {
  const t = q.trim().toLowerCase();
  return t ? ops.filter((o) => `${o.method} ${o.path} ${o.summary}`.toLowerCase().includes(t)) : ops;
};

/** Only parameterless reads can be run from the explorer: nothing here mutates, nothing needs a path value. */
export const runnable = (o: Operation): boolean => o.method === 'GET' && !o.path.includes('{') && !/\/(stream|ws|console|serial|tunnel)(\/|$)/.test(o.path);

/** The token is never inlined: the command reads it from the environment. */
export function curlFor(o: Operation, origin: string): string {
  const flags = o.method === 'GET' ? '' : ` -X ${o.method}`;
  return `curl -sS${flags} -H "Authorization: Bearer $FLUXVM_TOKEN" ${origin}${o.path}`;
}
