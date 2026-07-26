# Delivery route planner (v0.8.0)

CAVS is not one patch algorithm — it is a set of **routes** over the same
content-addressed release data. `cavs route-plan` picks the best route
for one concrete client state instead of forcing every client through
the same mechanism:

```text
no-op       already up to date                       0 bytes
chunks      warm cache / CDN range reads             fresh chunks only
hybrid      cold cache + previous install            fresh chunks only
cavsplan    offline stream patch                     plan bytes, ~40 MiB apply
cavspatch   optimized pairwise sidecar (hot pairs)   patch bytes, RAM varies
bootstrap   fresh install                            whole build, compressed
full        raw download                             whole build
```

```bash
cavs route-plan --installed ./Build_v1 --new ./Build_v2 \
  --plan releases/v1_to_v2.cavsplan \
  --patch releases/v1_to_v2.cavspatch \
  --profile low-memory --json
```

Routes backed by a real file (`--plan`, `--patch`, `--bootstrap`) are
priced exactly; the chunk/hybrid route is measured from the builds; the
rest are estimated and labelled as such. The chosen route is
`min(score)` where

```text
score = network_bytes · w_net + apply_ms · w_cpu + temp_disk · w_disk
        (routes over the profile's memory budget are excluded)
```

## Client profiles

| Profile | Memory budget | Weighting |
|---|---:|---|
| `default` | 1 GiB | smallest download wins |
| `low-memory` | 128 MiB | bsdiff-heavy sidecars excluded; streaming routes win |
| `slow-network` | 1 GiB | network bytes weighted 4× |
| `low-disk` | 1 GiB | temp disk weighted heavily |

A sidecar whose estimated apply peak exceeds the profile's budget is
marked `[excluded]` with the reason, and the planner falls back to the
offline plan — typically within a few percent of the sidecar's bytes at
a fraction of the memory.

## Client states it covers

- **already up to date** → `no-op` (verified, zero bytes);
- **fresh install, nothing local** → `bootstrap` (or `full`);
- **old install, warm cache** → `chunks`;
- **old install, no cache** → `hybrid` or `cavsplan`;
- **exact hot pair published** → `cavspatch` when it is actually cheaper
  *and* fits the device;
- **wrong-pair sidecar** → detected (recorded old size mismatch) and
  excluded, never mis-applied.

## Server-side shape

A server (or a static `chunk-map.json` export) advertises the same
inputs the offline planner uses: bootstrap URL, chunk-route estimate,
plan URL, sidecar URL + the exact old version it serves, and the apply
memory estimate. The client-side decision is this planner's scoring; no
route requires generating anything per client.

## Relation to the benchmarks

`cavs bench full-pipeline` reports a **CAVS auto-route** row computed
with the same policy (smallest payload; near-ties broken by apply time)
so comparisons against external pipelines are made against what the
planner would actually serve, not against a hand-picked route. See
[ROUTE_BENCHMARKS.md](ROUTE_BENCHMARKS.md).
