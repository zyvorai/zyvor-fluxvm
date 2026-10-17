// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

import { errorFromResponse, FluxVM } from "./client.js";

/** `GET /v1/agents/sessions/{id}/events`. */
export interface SessionEventsSnapshot {
  ledger: Record<string, unknown>[];
  vm_events: Record<string, unknown>[];
}

/**
 * One Server-Sent Event from `.../events/stream`. `ledger` is what the API did, `vm` what the
 * hypervisor saw, `error` ends the stream (the session is gone or the hour is up).
 */
export interface SessionStreamEvent {
  type: "ledger" | "vm" | "error";
  /** The parsed JSON payload. */
  data: Record<string, unknown>;
  /** The resume cursor, also sent back as `Last-Event-ID` on reconnect. */
  id?: string;
}

export interface WatchOptions {
  /** Replay only what is newer than this. Default: the whole history. */
  since?: Date | string;
  /** Resume from a cursor taken from an earlier event's `id`. */
  lastEventId?: string;
  /** Stop following when aborted. */
  signal?: AbortSignal;
  /** Give up after this many failed connection attempts in a row. Default 5. */
  maxReconnects?: number;
  /** Wait between reconnects, in milliseconds. Default 1000. */
  reconnectDelayMs?: number;
}

interface RawEvent {
  event: string;
  id?: string;
  data: string;
}

/** Split a byte stream into SSE events (`event:`, `id:`, `data:` lines, blank line ends one). */
export async function* parseSse(body: ReadableStream<Uint8Array>): AsyncGenerator<RawEvent> {
  const decoder = new TextDecoder();
  const reader = body.getReader();
  let buf = "";
  let cur: RawEvent = { event: "message", data: "" };
  let has = false;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });
      let nl: number;
      while ((nl = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, nl).replace(/\r$/, "");
        buf = buf.slice(nl + 1);
        if (line === "") {
          if (has) yield cur;
          cur = { event: "message", data: "" };
          has = false;
        } else if (!line.startsWith(":")) {
          const c = line.indexOf(":");
          const field = c < 0 ? line : line.slice(0, c);
          const val = c < 0 ? "" : line.slice(c + 1).replace(/^ /, "");
          if (field === "event") cur.event = val;
          else if (field === "id") cur.id = val;
          else if (field === "data") cur.data = cur.data ? `${cur.data}\n${val}` : val;
          else continue;
          has = true;
        }
      }
    }
  } finally {
    reader.releaseLock();
  }
}

const sleep = (ms: number, signal?: AbortSignal) =>
  new Promise<void>((resolve) => {
    const t = setTimeout(resolve, ms);
    signal?.addEventListener("abort", () => (clearTimeout(t), resolve()), { once: true });
  });

export async function* watchSessionEvents(
  fx: FluxVM,
  id: string,
  opts: WatchOptions,
): AsyncGenerator<SessionStreamEvent> {
  const maxFailures = opts.maxReconnects ?? 5;
  const delay = opts.reconnectDelayMs ?? 1000;
  let lastId = opts.lastEventId;
  let failures = 0;
  while (!opts.signal?.aborted) {
    const headers: Record<string, string> = { Accept: "text/event-stream" };
    let path = `/v1/agents/sessions/${encodeURIComponent(id)}/events/stream`;
    if (lastId !== undefined) headers["Last-Event-ID"] = lastId;
    else if (opts.since !== undefined) {
      const t = opts.since instanceof Date ? opts.since.toISOString() : opts.since;
      path += `?since=${encodeURIComponent(t)}`;
    }
    try {
      const resp = await fx.openStream(path, headers, opts.signal);
      if (!resp.ok || !resp.body) {
        const raw = new Uint8Array(await resp.arrayBuffer());
        const out: Record<string, string> = {};
        resp.headers.forEach((v, k) => (out[k.toLowerCase()] = v));
        const err = errorFromResponse(resp.status, raw, out);
        // A 4xx other than 429 will not get better by retrying.
        if (resp.status < 500 && resp.status !== 429) throw err;
        throw new StreamDropped(err.message);
      }
      failures = 0;
      for await (const raw of parseSse(resp.body)) {
        if (raw.id !== undefined) lastId = raw.id;
        let data: Record<string, unknown> = {};
        try {
          data = JSON.parse(raw.data) as Record<string, unknown>;
        } catch {
          // keep an empty payload for a frame that is not JSON
        }
        if (raw.event === "error") {
          // The hour limit asks the client to reconnect; anything else means the session is gone.
          if (String(data.error ?? "").startsWith("stream time limit")) break;
          yield { type: "error", data, id: raw.id };
          return;
        }
        if (raw.event === "ledger" || raw.event === "vm") yield { type: raw.event, data, id: raw.id };
      }
    } catch (err) {
      if (opts.signal?.aborted) return;
      if (!(err instanceof StreamDropped) && !isTransient(err)) throw err;
      if (++failures > maxFailures) throw err;
    }
    await sleep(delay, opts.signal);
  }
}

class StreamDropped extends Error {}

function isTransient(err: unknown): boolean {
  return err instanceof Error && (err.name === "ConnectionFailed" || err.name === "TypeError");
}
