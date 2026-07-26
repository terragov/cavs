# butler comparison (v0.7.0, complete pipeline in v0.8.0)

CAVS uses **butler as an external benchmark**. The harnesses do not
reimplement anything: they drive a local `butler` binary (`-j`
JSON-lines mode) through its real pipeline, capture the raw output,
measure wall time and peak RSS, and independently verify every applied
output byte-for-byte.

Two harnesses:

- `cavs bench butler-offline` (v0.7.0) — the **default patch** only:
  `diff` → `apply` → `verify`.
- `cavs bench butler-full` (v0.8.0) — the **complete pipeline**: `diff`,
  then `rediff --rediff-quality 9` (the strongest patch butler can
  generate — bsdiff-based with high-quality recompression), then apply
  and verify for *both* patches.

## What is being measured

butler's patching model has two phases:

1. `butler diff` computes a **default patch** with an rsync-style
   fixed-block algorithm and fast compression — optimized for quick
   generation and low memory.
2. `butler rediff` regenerates an **optimized patch** (bsdiff + brotli
   quality 9) from the same inputs — smaller, at a much higher CPU and
   memory cost.

v0.7.0 could only measure phase 1 and had to approximate phase 2 with
proxies. v0.8.0 measures both phases directly with butler's own
binaries, so the comparison covers the strongest patch the tool
produces — no proxies needed for the headline numbers (the
bsdiff/xdelta3 proxies remain available as an independent cross-check).

## Measured (butler v15.28.0, 126 MiB synthetic directory build)

| Metric | butler default | butler optimized (rediff q9) | CAVS auto-route (plan) |
|---|---:|---:|---:|
| patch size | 2.52 MiB (+62 KiB sig) | 2.51 MiB | **2.51 MiB** |
| generate time | 1.0 s | 11.6 s (diff+rediff) | **0.5 s** |
| apply time | 331 ms | 403 ms | **391 ms** |
| apply peak RSS | 35 MiB | 97 MiB | **23 MiB** |
| output byte-identical | yes | yes | yes |

The full per-route tables, including the shifted and compressed-blob
cases: [ROUTE_BENCHMARKS.md](ROUTE_BENCHMARKS.md).

## Different models, honestly framed

butler is an excellent **pairwise patching workflow**: a patch is
computed per old→new pair and later optimized.

CAVS optimizes a different shape:

- **content-addressed release store** — package once per release, every
  version jump (v1→v10, v3→v10, reinstall) is served from the same
  immutable objects with zero per-pair work;
- **persistent chunk cache** — bytes a client already fetched are never
  fetched again, across versions and titles;
- **hybrid reconstruction** — the previous install is a first-class byte
  source even with an empty cache;
- **CDN/object-storage friendly** — immutable packfiles, no smart server
  required;
- a **route planner** ([DELIVERY_PLANNER.md](DELIVERY_PLANNER.md)) that
  picks per client state, including optional **pairwise sidecars**
  ([PAIRWISE_SIDECARS.md](PAIRWISE_SIDECARS.md)) for hot pairs.

CAVS does not claim to beat butler in every metric on every input. It is
okay for a dedicated pairwise patch to win on bytes for one exact
old→new pair — the many-version benchmark (`cavs bench version-stream`)
shows where the store-once model wins instead: 10 versions of a 32 MiB
build fit in a **30.6 MiB** store that serves *any* jump directly, while
all-pairs one-hop coverage of 10 versions needs 45 patches plus full
artifacts. Since v1.1.0, `cavs bench patch-policy` extends this to the
*practical* pairwise policies (adjacent chains, sparse ladders, base
hubs, hot pairs) under explicit user traffic models, with
`butler-offline` available as a patch engine —
[PATCH_POLICY_BENCHMARK.md](PATCH_POLICY_BENCHMARK.md).
