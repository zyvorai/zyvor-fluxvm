# Fork-backed warm pools

Design notes for the remaining density item from `docs/ROADMAP-DENSITY.md`.

## Goal

`backfill_pool` currently cold-boots every missing member. A fork-backed
backfill holds one parked template VM per `PoolSpec` and creates the missing
members by calling `fork_vm` + `ResetIdentity`.

## Constraints

- Forked children inherit the parent's MAC and IP. The pool must therefore
  use per-VM netns (the same restriction `check_forkable` already enforces).
- The template must be fully quiesced after the agent is ready so children
  do not inherit in-flight requests.
- Identity reset (`AgentRequest::ResetIdentity`) must succeed; failure is
  currently logged but not fatal.

## Planned changes

1. Hold a single parked template VM per pool (instead of cold-starting each member).
2. On backfill, call the existing `fork_vm` path for each missing slot.
3. Run `ResetIdentity` on every child.
4. Keep the existing warm-pool claim / pause / resume semantics.

## Status

Design only. No code changes yet. This document is the place to track the
implementation once the uffd and dirty-log work is further along.
