# SteamPipe-style vs CAVS — measured comparison (v0.9.0)

How the fixed-1MiB SteamPipe-style model and the CAVS routes behave
across the update patterns that matter, measured on deterministic
datasets. Raw outputs and reproduction commands:
[results/v0.9.0/](results/v0.9.0/).

> Every SteamPipe-style figure below is an **estimate from a public
> model** — not Valve's exact SteamPipe implementation
> ([STEAMPIPE_STYLE_MODEL.md](STEAMPIPE_STYLE_MODEL.md)). bsdiff and
> xdelta3 are exact pairwise patchers included for reference; each patch
> serves exactly one old→new pair. Covering every jump in one hop costs
> O(N²) patches; practical systems chain adjacent/ladder/base policies
> instead — measured head-to-head in
> [PATCH_POLICY_BENCHMARK.md](PATCH_POLICY_BENCHMARK.md).

## Pack pathology (benchmarks A & B)

`cavs bench steampipe-cases`, seed 9, 32 × ~1 MiB assets per pack:

| Case | SteamPipe-style | Fixed reuse | CDC reuse | CAVS `.cavsplan` | bsdiff | Diagnosis |
|---|---:|---:|---:|---:|---:|---|
| pack-localized-small (64 KiB edit) | 1.00 MiB | 97.0% | 99.6% | 131 KiB | 65 KiB | localized / OK |
| pack-localized-medium (4 edits) | 4.00 MiB | 87.8% | 95.7% | 1.07 MiB | 805 KiB | localized / OK |
| pack-shifted (4 KiB head insert) | **32.88 MiB** | 0.0% | 99.8% | **7.4 KiB** | 4.6 KiB | asset_shuffling |
| pack-shuffled (same assets, new order) | **32.88 MiB** | 0.0% | 98.8% | **67 KiB** | 151 B | asset_shuffling |
| pack-toc-distributed (headers rewritten) | **32.00 MiB** | 2.7% | 91.4% | 2.13 MiB | 65 KiB | toc_churn |
| pack-toc-end (TOC centralized) | 1.88 MiB | 94.3% | 99.4% | 132 KiB | 65 KiB | localized / OK |
| pack-global-compressed | 194 KiB (whole blob) | 0.0% | 18.0% | 130 KiB | 65 KiB | compressed blob |
| pack-per-asset-compressed (padded slots) | 97 KiB | 75.0% | 97.4% | 68 KiB | 65 KiB | localized / OK |
| new-content-new-pack | 4.00 MiB (= new content) | 89.2% | 89.1% | 4.00 MiB | 4.00 MiB | localized / OK |

What the table shows:

- **Layout, not content, decides fixed-chunk update cost.** The same
  64 KiB of real change costs 1 MiB (localized), 1.9 MiB (TOC at the
  end) or the *entire pack* (shifted, shuffled, distributed TOC).
- **Content-defined chunking (CAVS) is immune to shifts** — the shifted
  pack costs 7.4 KiB instead of 32.88 MiB, with no per-pair patch.
- **The fixes work.** Centralizing the TOC turns a 32 MiB update into
  1.9 MiB under the same model; padded per-asset compression keeps 75%
  fixed reuse where a global stream keeps 0%.
- **New content as a new pack is free** — the update equals the new
  content itself under every route.

## Directory vs blob (benchmark C)

The same assets shipped as individual files (`directory-build`) cost
1.00 MiB under the fixed model — identical to the well-laid-out pack —
and reconstruct per file, so a failed update never leaves a torn pack
([DIRECTORY_MODE.md](DIRECTORY_MODE.md)).

## Godot PCK (benchmark G)

| Case | SteamPipe-style | CAVS `.cavsplan` | Note |
|---|---:|---:|---|
| PCK, one resource edited | 1.00 MiB | 128 KiB | localized |
| PCK, new resource packed first | 3.50 MiB (everything) | 1.06 MiB | offsets shifted |

`cavs analyze godot-pck` maps the changed byte ranges back to the
resource paths inside the PCK when the directory is parseable
([GODOT_PCK_ANALYZER.md](GODOT_PCK_ANALYZER.md)).

## Depot sharing (benchmark D)

Across windows/linux/demo/lang-es/hd-textures depots built from shared
content ([results/v0.9.0/depot-sharing/](results/v0.9.0/depot-sharing/)):

| Depot A | Depot B | Shared | Reuse |
|---|---|---:|---:|
| windows | linux | 48.83 MiB | 98.9% |
| windows | demo | 5.76 MiB | 11.7% |

Install plans by ownership: a windows-only client downloads 49.11 MiB;
a demo owner who already has the full build downloads **0 B** (every
chunk already local).

## Local disk I/O (benchmark F)

A ~3-byte change in a 256 MiB pack:

| Layout | Download | Local read+write | HDD estimate |
|---|---:|---:|---:|
| one 256 MiB pack | 2.00 MiB | **512 MiB** | 4.7 s |
| split into 8 × 32 MiB | 2.00 MiB | 128 MiB | 1.2 s |

Smaller download does not mean faster update; the fixed-chunk rebuild
re-reads and re-writes every touched pack in full
([IO_ESTIMATOR.md](IO_ESTIMATOR.md)).

## Many-version stream (benchmark E)

10 releases, ~3% drift: the CAVS content-addressed store holds all 10
versions in 22.43 MiB and serves any version jump with no extra server
work; pairwise patching would need 45 patches to serve every jump
directly ([results/v0.9.0/version-stream/](results/v0.9.0/version-stream/)).

## Route planner (benchmark H)

`cavs plan-update` recommendations across client states
([results/v0.9.0/route-planner/](results/v0.9.0/route-planner/)):

| Client state | Recommended route |
|---|---|
| cold install (incompressible data) | full download |
| previous install (any disk/RAM state) | `.cavsplan` (129 KiB vs 256 MiB) |

Unavailable routes (no sidecar generated, butler not installed) are
reported but never chosen ([ROUTE_PLANNER.md](ROUTE_PLANNER.md)).
