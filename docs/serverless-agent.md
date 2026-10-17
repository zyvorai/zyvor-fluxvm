# Using FluxVM from a serverless backend

A serverless backend (functions with a time limit, plus a database that pushes changes to its clients) can drive FluxVM
sandboxes and show their activity live. FluxVM has no plugin for any particular platform; this page shows the pattern
with the [TypeScript client](../typescript/README.md).

> **Not run on a serverless platform.** The two code blocks below were type-checked (strict) and run once against the
> client's in-memory `FakeFluxVM`, with a stub `db` and `schedule`: `run` creates, runs and deletes the sandbox and records
> the execution; `slice` writes the replayed and live events with the right `ownerId`, stops without rescheduling when the
> session ends, and schedules the next slice when the time slice runs out (the cursor it passes on was not checked). Everything platform-specific
> (how `db` stores rows, how `schedule` re-runs a function, secrets, time limits) is yours to adapt and is untested. That
> check is not committed as a test.

## Pieces

| Need | Use |
|---|---|
| Create, run, delete a sandbox from a function | `FluxVM.createSandbox`, `sb.run`, `sb.delete` |
| Live activity in your UI | follow `GET /v1/agents/sessions/{id}/events/stream`, write each event into a table, let your UI subscribe to that table |
| One user never sees another's sandbox | keep an `ownerId` on your own rows and filter every query by it; also give each tenant its own FluxVM token, so FluxVM answers `404` for ids that are not theirs |
| Branch on failures | `err.code` (`NOT_FOUND`, `CONFLICT`, `RATE_LIMITED`, ...), see [Errors](api.md#errors) |
| Safe retries | send an `Idempotency-Key` on create, fork, snapshot and delete ([details](api.md#idempotency-key)) |

## Running a command

Keep the FluxVM URL and token in the platform's secret store, never in function arguments.

```ts
import { FluxVM } from "@zyvorai/fluxvm";

const fx = () => new FluxVM({ baseUrl: process.env.FLUXVM_URL!, token: process.env.FLUXVM_TOKEN! });

export async function run(ownerId: string, command: string) {
  // withSandbox deletes the sandbox when the callback returns or throws.
  return fx().withSandbox({ name: `job-${ownerId}`, ttlSeconds: 600 }, async (sb) => {
    const r = await sb.run(command, { timeoutSeconds: 60 });
    await db.executions.insert({ ownerId, command, ...r });
    return r;
  });
}
```

Functions usually stop after a few minutes, so keep `timeoutSeconds` well under that and start long jobs through the
session `jobs` API, then poll or stream them.

## Live events

A function cannot hold one connection open for an hour, so follow the stream in short slices and keep the resume
cursor in a table. `watchSessionEvents` reconnects with `Last-Event-ID`, so no ledger entry is repeated or missed
between slices (a VM event with exactly the same timestamp as the last one may repeat; store a key to drop it).

```ts
export async function slice(ownerId: string, sessionId: string, cursor?: string) {
  const stop = new AbortController();
  const timer = setTimeout(() => stop.abort(), 4 * 60_000); // stay under your function's time limit
  let last = cursor;
  let ended = false;
  for await (const e of fx().watchSessionEvents(sessionId, { lastEventId: cursor, signal: stop.signal })) {
    if (e.type === "error") ended = true; // the session is gone
    else last = e.id ?? last;
    await db.events.insert({ ownerId, sessionId, type: e.type, data: e.data });
  }
  clearTimeout(timer);
  if (!ended) await schedule(() => slice(ownerId, sessionId, last)); // your platform's way to run it again
}
```

A UI that subscribes to `db.events` filtered by `ownerId` and `sessionId` then updates as rows arrive.

## What this does not give you

- No wrapped lifecycle verbs (`pause`, `hibernate`, `wake`); call the FluxVM REST routes you need.
- The FluxVM token sits in your backend's secrets, so all your users share its permissions. Per-user isolation comes from
  your `ownerId` checks, or from minting one FluxVM tenant token per user.
