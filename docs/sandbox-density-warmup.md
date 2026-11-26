# Predictive warm pool and density-aware placement

## Predictive warm pool

Instead of a static list of sizes, the daemon watches what you actually run and keeps the hottest combinations warm.

```toml
[apple]
oci_warm_slots = 4
oci_warm_predictive = true   # learn from recent creates
```

After a few runs of `python:3.13` at 1x512, the next one starts in <1 s instead of the cold-boot time.

## Density-aware placement

When you have a fleet of Macs:

```bash
fluxctl sandbox run alpine:3.22 --fleet https://central:7788
```

The central agent prefers the Mac with the most free density slots (not just free CPU/memory). This keeps the warm pools effective.

## Metrics

`GET /v1/sandboxes/density` already reports warm hits/misses. This adds:

- `predictive_hits`
- `top_images` (what the predictor is keeping warm)
- `placement_reason` on each create

## Why this is better than Docker

Docker has no warm pool and no density-aware multi-host placement for containers. You get cold starts every time and simple round-robin or least-loaded scheduling. FluxVM can keep the common cases hot and place intelligently.

Implementation builds on the existing warm pool and fleet placement.
