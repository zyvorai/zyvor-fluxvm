// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

import assert from "node:assert/strict";
import http from "node:http";
import type { AddressInfo } from "node:net";
import { after, before, beforeEach, describe, it } from "node:test";
import { AuthError, Conflict, FleetClient, ForbiddenError, NotFound, RateLimited, ServiceUnavailable } from "../src/index.js";

interface Seen {
  method: string;
  url: string;
  headers: http.IncomingHttpHeaders;
  body: unknown;
}

type Answer = [number, unknown, Record<string, string>?];

let server: http.Server;
let base: string;
let seen: Seen[] = [];
/** Routes for the current test: "METHOD /path" (query ignored) -> one answer, or a list consumed in order. */
let routes: Record<string, Answer | Answer[]> = {};

before(async () => {
  server = http.createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on("data", (c: Buffer) => chunks.push(c));
    req.on("end", () => {
      const raw = Buffer.concat(chunks).toString();
      const s: Seen = { method: req.method!, url: req.url!, headers: req.headers, body: raw ? JSON.parse(raw) : undefined };
      seen.push(s);
      const key = `${s.method} ${s.url.split("?")[0]}`;
      let answer = routes[key];
      if (Array.isArray(answer) && Array.isArray(answer[0])) {
        const list = answer as Answer[];
        answer = list.length > 1 ? list.shift()! : list[0];
      }
      const [status, body, headers] = (answer as Answer | undefined) ?? [404, { error: `no route ${key}` }];
      if (status === 204) {
        res.writeHead(204);
        res.end();
        return;
      }
      const text = typeof body === "string" ? body : JSON.stringify(body);
      res.writeHead(status, { "content-type": typeof body === "string" ? "text/plain" : "application/json", ...(headers ?? {}) });
      res.end(text);
    });
  });
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
  base = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
});

after(() => {
  server.closeAllConnections();
  server.close();
});

beforeEach(() => {
  seen = [];
  routes = {};
});

const sleeps: number[] = [];
const fleet = (token = "fvt_acme") =>
  new FleetClient({ baseUrl: base + "/", token, sleep: async (ms) => void sleeps.push(ms) });

const vm = { id: "0b9c", name: "ci-1", node: "mac-2", backend: "vz", status: "running", created_at: "t", expires_at: null, error: null };
const image = {
  name: "macos-27-xcode", display_name: "macOS 27", description: null, guest_os: "macos",
  image: "/Users/fluxvm/vms/xcode/disk.raw", vcpus: 6, memory_mib: 16384, disk_size_gib: null, source: null, updated_at: null,
};

describe("FleetClient", () => {
  it("sends the bearer token and maps errors", async () => {
    routes["GET /fleet/tenants/me"] = [200, { id: "acme", quota: {}, disabled: false }];
    assert.equal((await fleet().me()).id, "acme");
    assert.equal(seen[0].headers.authorization, "Bearer fvt_acme");
    assert.match(String(seen[0].headers["user-agent"]), /^fluxvm-fleet-ts\//);

    routes["GET /fleet/tenants"] = [403, { error: "tenant 'acme' may not call GET /fleet/tenants" }];
    await assert.rejects(fleet().listTenants(), (e: unknown) => e instanceof ForbiddenError && e.detail.includes("may not call"));
    routes["GET /fleet/tenants/me"] = [401, { error: "missing or invalid bearer token" }];
    await assert.rejects(fleet("bad").me(), AuthError);
  });

  it("leases, launches from the catalog, runs and deletes", async () => {
    routes["POST /fleet/leases"] = [201, { lease: { id: "lease-1", node: "mac-2", state: "active" }, node: { name: "mac-2" } }];
    routes["GET /fleet/images"] = [200, { items: [image] }];
    routes["POST /fleet/vms"] = [200, { node: "mac-2", vm }];
    routes["GET /fleet/vms"] = [200, { items: [vm], unreachable_nodes: [{ node: "mac-9", reason: "did not answer" }] }];
    routes["POST /fleet/vms/mac-2/0b9c/exec"] = [200, { exit_code: 0, stdout: "27.0\n", stderr: "" }];
    routes["POST /fleet/vms/mac-2/0b9c/stop"] = [200, { ...vm, status: "stopped" }];
    routes["DELETE /fleet/vms/mac-2/0b9c"] = [204, null];
    routes["GET /fleet/vms/mac-2/0b9c/logs"] = [200, "boot\nlogin:\n"];
    routes["POST /fleet/leases/lease-1/release"] = [409, { error: "lease cannot be released before … (24-hour minimum)" }];

    const c = fleet();
    const { lease } = await c.createLease({ hours: 48, nested_virtualization: true });
    assert.deepEqual(seen.at(-1)!.body, { hours: 48, nested_virtualization: true });
    const res = await c.launchFromCatalog("ci-1", "macos-27-xcode", { networks: ["build"], volumes: ["cache"], extra: { labels: { job: "42" } } });
    assert.deepEqual(seen.at(-1)!.body, {
      labels: { job: "42" }, name: "ci-1", backend: "vz", image: "/Users/fluxvm/vms/xcode/disk.raw",
      apple: { guest_os: "macos", networks: [{ name: "build" }] }, vcpus: 6, memory_mib: 16384, volumes: ["cache"],
    });
    assert.equal(res.node, "mac-2");
    assert.equal((await c.getVm("mac-2", "0b9c")).name, "ci-1");
    assert.equal((await c.findVm("ci-1")).id, "0b9c");
    await assert.rejects(c.getVm("mac-9", "x"), ServiceUnavailable);
    await assert.rejects(c.getVm("mac-2", "nope"), NotFound);
    assert.equal((await c.execVm("mac-2", "0b9c", "sw_vers", 30)).stdout, "27.0\n");
    assert.deepEqual(seen.at(-1)!.body, { command: "sw_vers", timeout_seconds: 30 });
    assert.equal((await c.powerVm("mac-2", "0b9c", "stop")).status, "stopped");
    assert.match(await c.vmLogs("mac-2", "0b9c", 20), /login:/);
    assert.equal(seen.at(-1)!.url, "/fleet/vms/mac-2/0b9c/logs?lines=20");
    await c.deleteVm("mac-2", "0b9c");
    await assert.rejects(c.releaseLease(lease.id), (e: unknown) => e instanceof Conflict && e.status === 409);
  });

  it("keys, webhooks, billing, volumes and pools", async () => {
    routes["POST /fleet/tenants/me/keys"] = [201, { key: { id: "key_0123456789ab", name: "ci" }, token: "fvk_x", note: "" }];
    routes["DELETE /fleet/tenants/me/keys/ci"] = [200, { revoked: { id: "key_0123456789ab", name: "ci" } }];
    routes["POST /fleet/webhooks"] = [201, { webhook: { id: "whk_1" }, secret: "whsec_abc", note: "" }];
    routes["POST /fleet/webhooks/whk_1/test"] = [200, { delivery: { ok: true, status: 200 } }];
    routes["GET /fleet/billing"] = [200, { from: "a", to: "b", currency: "USD", prices_set: true, statements: [] }];
    routes["PUT /fleet/tenants/me/budget"] = [200, { id: "acme", monthly_budget: 500 }];
    routes["POST /fleet/volumes"] = [201, { id: "vol-1", name: "cache", node: "mac-2", size_gib: 50, attached_to: null }];
    routes["POST /fleet/volumes/cache/resize"] = [200, { id: "vol-1", name: "cache", node: "mac-2", size_gib: 80, attached_to: null }];
    routes["POST /fleet/pools"] = [201, { name: "ci", image: "macos-27-xcode", min_ready: 1 }];
    routes["PATCH /fleet/pools/ci"] = [200, { name: "ci", image: "macos-27-xcode", min_ready: 2 }];
    routes["DELETE /fleet/pools/ci"] = [204, null];

    const c = fleet();
    assert.equal((await c.createKey("ci", { expiresInDays: 30 })).token, "fvk_x");
    assert.deepEqual(seen.at(-1)!.body, { name: "ci", read_only: false, expires_in_days: 30 });
    assert.equal((await c.revokeKey("ci")).name, "ci");
    assert.equal((await c.createWebhook("https://hooks.example.com/x", ["lease.ending"])).secret, "whsec_abc");
    assert.ok((await c.testWebhook("whk_1")).ok);
    await c.billing({ month: "2026-10" });
    assert.equal(seen.at(-1)!.url, "/fleet/billing?month=2026-10");
    assert.equal((await c.setBudget(500)).monthly_budget, 500);
    assert.equal((await c.createVolume("cache", 50)).size_gib, 50);
    assert.equal((await c.resizeVolume("cache", 80)).size_gib, 80);
    assert.deepEqual(seen.at(-1)!.body, { size_gib: 80 });
    await c.createPool("ci", { image: "macos-27-xcode", min_ready: 1 });
    assert.equal((await c.updatePool("ci", { min_ready: 2 })).min_ready, 2);
    await c.deletePool("ci");
  });

  it("retries idempotent calls on 429 and 5xx, never POSTs", async () => {
    routes["GET /fleet/images"] = [[503, { error: "busy" }], [502, { error: "busy" }], [200, { items: [image] }]];
    sleeps.length = 0;
    assert.equal((await fleet().listImages()).length, 1);
    assert.equal(seen.length, 3);
    assert.equal(sleeps.length, 2);

    seen = [];
    routes["GET /fleet/capacity"] = [429, { error: "slow down" }, { "retry-after": "2" }];
    sleeps.length = 0;
    await assert.rejects(fleet().capacity(), RateLimited);
    assert.equal(seen.length, 4);
    assert.ok(sleeps.every((ms) => ms >= 2000), `Retry-After honoured: ${sleeps}`);

    seen = [];
    routes["POST /fleet/leases"] = [502, { error: "node unreachable" }];
    await assert.rejects(fleet().createLease(), (e: unknown) => (e as { status?: number }).status === 502);
    assert.equal(seen.length, 1);
  });
});
