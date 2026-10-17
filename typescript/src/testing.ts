// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

/**
 * An in-memory stand-in for a FluxVM daemon, for your own unit tests. It speaks just enough of
 * the REST API for the client: create, list, get and delete sandboxes, `process`, and the
 * session `events` and `events/stream` routes. Nothing boots; commands are answered by a
 * function you give it. It is not a model of the real server, so run a live test before you
 * ship.
 *
 *     const fake = await FakeFluxVM.start();
 *     const fx = new FluxVM({ baseUrl: fake.url, token: "t" });
 *     fake.emit("sess-1", "ledger", { op: "exec" });
 */
import http from "node:http";
import type { AddressInfo } from "node:net";

export interface FakeOptions {
  /** Answer for `POST /v1/sandboxes/{id}/process`. Default: exit 0 with empty output. */
  run?: (command: string) => { exitCode?: number; stdout?: string; stderr?: string };
  /** token -> tenant. When empty, every request is accepted as tenant `default`. */
  tokens?: Record<string, string>;
}

interface Sb {
  record: Record<string, unknown>;
  tenant: string;
}

interface Session {
  tenant: string;
  ledger: Record<string, unknown>[];
  vm: Record<string, unknown>[];
  streams: Set<http.ServerResponse>;
}

export class FakeFluxVM {
  readonly url: string;
  private readonly server: http.Server;
  private readonly sandboxes = new Map<string, Sb>();
  private readonly sessions = new Map<string, Session>();
  private seq = 0;

  private constructor(server: http.Server, private readonly opts: FakeOptions) {
    this.server = server;
    this.url = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
  }

  static async start(opts: FakeOptions = {}): Promise<FakeFluxVM> {
    let self: FakeFluxVM;
    const server = http.createServer((req, res) => self.handle(req, res));
    await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
    self = new FakeFluxVM(server, opts);
    return self;
  }

  /** Create a session owned by `tenant` (default `default`) so it can be watched. */
  session(id: string, tenant = "default"): void {
    if (!this.sessions.has(id)) this.sessions.set(id, { tenant, ledger: [], vm: [], streams: new Set() });
  }

  /** Append a ledger or VM event; open streams receive it at once. Creates the session if needed. */
  emit(sessionId: string, type: "ledger" | "vm", data: Record<string, unknown>): void {
    this.session(sessionId);
    const s = this.sessions.get(sessionId)!;
    (type === "ledger" ? s.ledger : s.vm).push(data);
    for (const res of s.streams) this.write(res, s, type, data);
  }

  /** Delete a session: open streams get an `error` event and close, like the real server. */
  endSession(sessionId: string): void {
    const s = this.sessions.get(sessionId);
    if (!s) return;
    for (const res of s.streams) {
      res.write(`event: error\ndata: ${JSON.stringify({ error: "session not found" })}\n\n`);
      res.end();
    }
    this.sessions.delete(sessionId);
  }

  /** Close the listener and every open stream. */
  async stop(): Promise<void> {
    this.server.closeAllConnections();
    await new Promise<void>((r) => this.server.close(() => r()));
  }

  private write(res: http.ServerResponse, s: Session, type: string, data: Record<string, unknown>): void {
    res.write(`event: ${type}\nid: ${s.ledger.length}|\ndata: ${JSON.stringify(data)}\n\n`);
  }

  private fail(res: http.ServerResponse, status: number, code: string, message: string): void {
    res.writeHead(status, { "content-type": "application/json" });
    res.end(JSON.stringify({ error: message, code }));
  }

  private ok(res: http.ServerResponse, status: number, body: unknown): void {
    res.writeHead(status, { "content-type": "application/json" });
    res.end(JSON.stringify(body));
  }

  private tenantOf(req: http.IncomingMessage): string | null {
    const tokens = this.opts.tokens;
    if (!tokens || Object.keys(tokens).length === 0) return "default";
    const m = /^Bearer (.+)$/.exec(req.headers.authorization ?? "");
    return m && tokens[m[1]] !== undefined ? tokens[m[1]] : null;
  }

  private handle(req: http.IncomingMessage, res: http.ServerResponse): void {
    const chunks: Buffer[] = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => {
      const tenant = this.tenantOf(req);
      if (tenant === null) return this.fail(res, 401, "UNAUTHORIZED", "missing or invalid bearer token");
      const url = new URL(req.url ?? "/", "http://x");
      const body = chunks.length ? JSON.parse(Buffer.concat(chunks).toString() || "{}") : {};
      this.route(req.method ?? "GET", url, body, tenant, req, res);
    });
  }

  private route(
    method: string,
    url: URL,
    body: Record<string, unknown>,
    tenant: string,
    req: http.IncomingMessage,
    res: http.ServerResponse,
  ): void {
    const path = url.pathname;
    if (method === "GET" && path === "/healthz") return this.ok(res, 200, { ok: true });
    if (method === "POST" && path === "/v1/sandboxes") {
      const id = `sb-${++this.seq}`;
      const record = { id, name: body.name ?? "", status: "running", backend: "fake", guest_ip: "10.0.0.2" };
      this.sandboxes.set(id, { record, tenant });
      return this.ok(res, 201, record);
    }
    if (method === "GET" && path === "/v1/sandboxes") {
      const items = [...this.sandboxes.values()].filter((s) => s.tenant === tenant).map((s) => s.record);
      return this.ok(res, 200, { items });
    }
    let m = /^\/v1\/vms\/([^/]+)$/.exec(path);
    if (m) {
      const sb = this.sandboxes.get(m[1]);
      if (!sb || sb.tenant !== tenant) return this.fail(res, 404, "NOT_FOUND", "VM not found");
      if (method === "DELETE") {
        this.sandboxes.delete(m[1]);
        res.writeHead(204);
        return void res.end();
      }
      return this.ok(res, 200, sb.record);
    }
    m = /^\/v1\/sandboxes\/([^/]+)\/process$/.exec(path);
    if (m && method === "POST") {
      const sb = this.sandboxes.get(m[1]);
      if (!sb || sb.tenant !== tenant) return this.fail(res, 404, "NOT_FOUND", "VM not found");
      const cmd = Array.isArray(body.command) ? (body.command as string[]).join(" ") : String(body.command ?? "");
      const r = this.opts.run?.(cmd) ?? {};
      return this.ok(res, 200, {
        result: "exec",
        exit_code: r.exitCode ?? 0,
        stdout: r.stdout ?? "",
        stderr: r.stderr ?? "",
      });
    }
    m = /^\/v1\/agents\/sessions\/([^/]+)\/events(\/stream)?$/.exec(path);
    if (m && method === "GET") {
      const s = this.sessions.get(decodeURIComponent(m[1]));
      // Another tenant's session answers 404, never 403, like the real server.
      if (!s || s.tenant !== tenant) return this.fail(res, 404, "NOT_FOUND", "session not found");
      if (!m[2]) return this.ok(res, 200, { ledger: s.ledger, vm_events: s.vm });
      const resume = /^(\d+)\|/.exec(String(req.headers["last-event-id"] ?? ""));
      const skip = resume ? Number(resume[1]) : 0;
      res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
      s.ledger.slice(skip).forEach((e, i) => res.write(`event: ledger\nid: ${skip + i + 1}|\ndata: ${JSON.stringify(e)}\n\n`));
      s.vm.forEach((e) => this.write(res, s, "vm", e));
      s.streams.add(res);
      res.on("close", () => s.streams.delete(res));
      return;
    }
    this.fail(res, 404, "NOT_FOUND", `no route ${method} ${path}`);
  }
}
