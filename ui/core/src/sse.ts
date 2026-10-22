import { apiBase, getToken, Unauthorized } from './api';

export interface SseMessage {
  event: string;
  data: unknown;
}

/**
 * Server-sent events over fetch: `EventSource` cannot send an Authorization header, and the bearer must never
 * go in the URL. Reconnects with backoff until `signal` aborts.
 */
export async function streamEvents(path: string, onMessage: (m: SseMessage) => void, signal: AbortSignal): Promise<void> {
  let delay = 1000;
  while (!signal.aborted) {
    try {
      const token = getToken();
      const res = await fetch(apiBase() + path, {
        headers: { Accept: 'text/event-stream', ...(token ? { Authorization: `Bearer ${token}` } : {}) },
        signal,
        cache: 'no-store',
      });
      if (res.status === 401) throw new Unauthorized();
      if (!res.ok || !res.body) throw new Error(`stream ${res.status}`);
      delay = 1000;
      const reader = res.body.pipeThrough(new TextDecoderStream()).getReader();
      let buf = '';
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        buf += value;
        let i;
        while ((i = buf.indexOf('\n\n')) >= 0) {
          const msg = parseSse(buf.slice(0, i));
          buf = buf.slice(i + 2);
          if (msg) onMessage(msg);
        }
      }
    } catch (e) {
      if (signal.aborted || e instanceof Unauthorized) return;
    }
    await new Promise((r) => setTimeout(r, delay));
    delay = Math.min(delay * 2, 15000);
  }
}

export function parseSse(block: string): SseMessage | null {
  let event = 'message';
  const data: string[] = [];
  for (const line of block.split('\n')) {
    if (line.startsWith('event:')) event = line.slice(6).trim();
    else if (line.startsWith('data:')) data.push(line.slice(5).trimStart());
  }
  if (!data.length) return null;
  const raw = data.join('\n');
  try {
    return { event, data: JSON.parse(raw) };
  } catch {
    return { event, data: raw };
  }
}
