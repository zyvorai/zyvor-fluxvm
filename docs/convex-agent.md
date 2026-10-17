# Using FluxVM from Convex

[Convex](https://www.convex.dev) backends can drive FluxVM sandboxes from actions and show their activity live,
because Convex queries re-run when a table changes. FluxVM has no Convex component; this page shows the pattern with
the [TypeScript client](../typescript/README.md).

> **Not run against a Convex deployment.** The FluxVM calls are covered by the client's tests (against the in-memory
> `FakeFluxVM`); the Convex glue below is a sketch to adapt, not a tested package.

## Pieces

| Need | Use |
|---|---|
| Create, run, delete a sandbox from a Convex action | `FluxVM.createSandbox`, `sb.run`, `sb.delete` |
| Live activity in your UI | follow `GET /v1/agents/sessions/{id}/events/stream`, write each event into a Convex table, read it with a query |
| One user never sees another's sandbox | keep `ownerId` on your own rows and pass it to every query; also give each tenant its own FluxVM token, so FluxVM answers `404` for ids that are not theirs |
| Branch on failures | `err.code` (`NOT_FOUND`, `CONFLICT`, `RATE_LIMITED`, ...), see [Errors](api.md#errors) |
| Safe retries | send an `Idempotency-Key` on create, fork, snapshot and delete ([details](api.md#idempotency-key)) |

## Actions

Use the Node runtime (the client uses `Buffer`). Keep the FluxVM URL and token in Convex environment variables
(`npx convex env set FLUXVM_TOKEN ...`), never in arguments.

```ts
// convex/sandbox.ts
"use node";
import { v } from "convex/values";
import { action } from "./_generated/server";
import { internal } from "./_generated/api";
import { FluxVM } from "@zyvorai/fluxvm";

const fx = () => new FluxVM({ baseUrl: process.env.FLUXVM_URL!, token: process.env.FLUXVM_TOKEN! });

export const run = action({
  args: { ownerId: v.string(), command: v.string() },
  handler: async (ctx, { ownerId, command }) => {
    // withSandbox deletes the sandbox when the callback returns or throws.
    return await fx().withSandbox({ name: `convex-${ownerId}`, ttlSeconds: 600 }, async (sb) => {
      const r = await sb.run(command, { timeoutSeconds: 60 });
      await ctx.runMutation(internal.executions.record, { ownerId, command, ...r });
      return r;
    });
  },
});
```

A Convex action stops after 10 minutes, so keep `timeoutSeconds` well under that and start long jobs through the
session `jobs` API, then poll or stream them.

## Live events

A Convex action cannot hold one connection open for an hour, so follow the stream in short slices and keep the
resume cursor in a table. `watchSessionEvents` reconnects with `Last-Event-ID`, so no ledger entry is repeated or
missed between slices (a VM event with exactly the same timestamp as the last one may repeat; store a key to drop it).

```ts
// convex/watch.ts
"use node";
import { v } from "convex/values";
import { internalAction } from "./_generated/server";
import { internal } from "./_generated/api";
import { FluxVM } from "@zyvorai/fluxvm";

export const slice = internalAction({
  args: { ownerId: v.string(), sessionId: v.string(), cursor: v.optional(v.string()) },
  handler: async (ctx, { ownerId, sessionId, cursor }) => {
    const fx = new FluxVM({ baseUrl: process.env.FLUXVM_URL!, token: process.env.FLUXVM_TOKEN! });
    const stop = new AbortController();
    const timer = setTimeout(() => stop.abort(), 8 * 60_000); // stay under the 10-minute limit
    let last = cursor;
    let ended = false;
    for await (const e of fx.watchSessionEvents(sessionId, { lastEventId: cursor, signal: stop.signal })) {
      if (e.type === "error") ended = true; // the session is gone
      else last = e.id ?? last;
      await ctx.runMutation(internal.events.append, { ownerId, sessionId, type: e.type, data: e.data });
    }
    clearTimeout(timer);
    if (!ended) await ctx.scheduler.runAfter(0, internal.watch.slice, { ownerId, sessionId, cursor: last });
  },
});
```

The `events.append` mutation inserts a row; a query such as `events.list({ ownerId, sessionId })` then updates in the
UI as rows arrive. Filter every query by `ownerId`.

## What this does not give you

- No Convex-side lifecycle verbs (`pause`, `hibernate`, `wake`) wrapped for you; call the FluxVM REST routes you need.
- The FluxVM token sits in the Convex environment, so all your users share its permissions. Per-user isolation comes from
  your `ownerId` checks, or from minting one FluxVM tenant token per user.
