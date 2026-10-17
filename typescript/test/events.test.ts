// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

import assert from "node:assert/strict";
import { after, before, describe, it } from "node:test";
import { ApiError, Conflict, FluxVM, NotFound, type SessionStreamEvent } from "../src/index.js";
import { FakeFluxVM } from "../src/testing.js";

let fake: FakeFluxVM;
let fx: FluxVM;

before(async () => {
  fake = await FakeFluxVM.start({
    tokens: { a: "acme", b: "other" },
    run: (cmd) => ({ stdout: `ran ${cmd}\n` }),
  });
  fx = new FluxVM({ baseUrl: fake.url, token: "a" });
});

after(() => fake.stop());

async function take(it: AsyncGenerator<SessionStreamEvent>, n: number): Promise<SessionStreamEvent[]> {
  const out: SessionStreamEvent[] = [];
  for await (const e of it) {
    out.push(e);
    if (out.length === n) break;
  }
  return out;
}

describe("watchSessionEvents", () => {
  it("replays history, then follows new events in order", async () => {
    fake.session("s1", "acme");
    fake.emit("s1", "ledger", { op: "create" });
    const ctl = new AbortController();
    const watch = fx.watchSessionEvents("s1", { signal: ctl.signal });
    const got = take(watch, 3);
    await new Promise((r) => setTimeout(r, 50));
    fake.emit("s1", "vm", { event: "vm.start" });
    fake.emit("s1", "ledger", { op: "exec" });
    const events = await got;
    ctl.abort();
    assert.deepEqual(
      events.map((e) => [e.type, e.data.op ?? e.data.event]),
      [["ledger", "create"], ["vm", "vm.start"], ["ledger", "exec"]],
    );
  });

  it("resumes from a cursor without repeating ledger entries", async () => {
    fake.session("s2", "acme");
    fake.emit("s2", "ledger", { op: "one" });
    fake.emit("s2", "ledger", { op: "two" });
    const ctl = new AbortController();
    const first = await take(fx.watchSessionEvents("s2", { signal: ctl.signal }), 1);
    ctl.abort();
    const cursor = first[0].id;
    assert.ok(cursor);
    const ctl2 = new AbortController();
    const next = await take(fx.watchSessionEvents("s2", { lastEventId: cursor, signal: ctl2.signal }), 1);
    ctl2.abort();
    assert.equal(next[0].data.op, "two");
  });

  it("yields a final error event when the session goes away, then ends", async () => {
    fake.session("s3", "acme");
    const events: SessionStreamEvent[] = [];
    const done = (async () => {
      for await (const e of fx.watchSessionEvents("s3")) events.push(e);
    })();
    await new Promise((r) => setTimeout(r, 50));
    fake.endSession("s3");
    await done;
    assert.deepEqual(events.map((e) => e.type), ["error"]);
  });

  it("does not retry a 404, and another tenant's session is a 404", async () => {
    fake.session("s4", "other");
    await assert.rejects(take(fx.watchSessionEvents("s4"), 1), (e: unknown) => {
      assert.ok(e instanceof NotFound);
      assert.equal((e as NotFound).code, "NOT_FOUND");
      return true;
    });
  });
});

describe("errors and the fake server", () => {
  it("exposes the server's stable code", async () => {
    await assert.rejects(fx.sessionEvents("missing"), (e: unknown) => {
      assert.ok(e instanceof ApiError);
      assert.equal((e as ApiError).code, "NOT_FOUND");
      return true;
    });
  });

  it("maps 409 to Conflict", async () => {
    const odd = new FluxVM({
      baseUrl: fake.url,
      fetch: async () => new Response(JSON.stringify({ error: "busy", code: "CONFLICT" }), { status: 409 }),
    });
    await assert.rejects(odd.listSandboxes(), (e: unknown) => e instanceof Conflict && e.code === "CONFLICT");
  });

  it("runs a sandbox end to end and keeps tenants apart", async () => {
    const sb = await fx.createSandbox({ name: "demo" });
    assert.equal((await sb.run("echo hi")).stdout, "ran echo hi\n");
    const other = new FluxVM({ baseUrl: fake.url, token: "b" });
    assert.equal((await other.listSandboxes()).length, 0);
    await assert.rejects(other.getSandbox(sb.info.id), NotFound);
    await sb.delete();
    await assert.rejects(new FluxVM({ baseUrl: fake.url, token: "nope" }).listSandboxes(), { name: "AuthError" });
  });
});
