# Traffic models — simulated user update behavior (v1.1.0)

A patch policy comparison is meaningless without an update-behavior
assumption: adjacent diffs look perfect if every client updates every
release, and terrible if half of them return after five patches. The
patch policy benchmark makes that assumption explicit and prices every
policy under it.

## Built-in models

| Model | adjacent | skip | old→latest | reinstall |
|---|---:|---:|---:|---:|
| `adjacent-heavy` | 80% | 15% (skip 2–4) | 4% (age ≥6) | 1% |
| `skip-heavy` | 40% | 40% (skip 2–8) | 15% (age ≥6) | 5% |
| `live-service-weekly` | 65% | 25% (skip 2–3) | 8% (age ≥6) | 2% |
| `major-release` | 30% | 20% (skip 2–5) | 45% (age ≥4) | 5% |
| `random` | — | 100% (any distance) | — | — |

All default to 100,000 users (`--users` overrides).

## Custom models

`--traffic-model custom:traffic.toml`:

```toml
[traffic]
name = "adjacent-heavy-live-service"
users = 100000

[[traffic.rule]]
kind = "adjacent"
probability = 0.70

[[traffic.rule]]
kind = "skip_range"
min_skip = 2
max_skip = 5
probability = 0.20

[[traffic.rule]]
kind = "old_to_latest"
min_age = 6
probability = 0.08

[[traffic.rule]]
kind = "reinstall_latest"
probability = 0.02
```

Rule kinds:

- `adjacent` — `vi→vi+1` for every i;
- `skip_range` — `vi→vj` with `min_skip ≤ j−i ≤ max_skip`;
- `old_to_latest` — `vi→vN` with `N−i ≥ min_age`;
- `reinstall_latest` — a fresh full install of the latest version.

## Expansion semantics

Expansion is deterministic (no sampling): each rule's probability is
spread uniformly across the (from,to) pairs it matches; rules that
match nothing on a short stream are dropped and the rest renormalized
to sum to 1; duplicate pairs across rules are merged. The result is a
weighted query set — `traffic_report.md` prints it in full, and
averages/percentiles in every report are probability-weighted, with
`total_served_bytes = users × expected bytes per query`.

Reinstalls are priced as a full compressed download for pairwise
policies and a full chunk-store install for CAVS.

## Choosing a model

Use `adjacent-heavy` for the most favorable realistic case for adjacent
pairwise diffs, `skip-heavy` to stress chains and long jumps,
`live-service-weekly` for frequent-release products, `major-release` for
big returning waves. Replaying a measured graph under a different model
costs no re-diffing:

```sh
cavs patch-policy simulate --graph results/patch-policy/patch_graph.json \
  --traffic-model skip-heavy
```
