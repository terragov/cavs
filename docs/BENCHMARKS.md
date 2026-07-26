# Benchmarks

All numbers are **measured**, not projected. Update payloads were captured over
real HTTP sessions (`cavs-server` + `cavs-client`). The corpus is real
open-source Godot projects exported to PCK at two real points in their git
history — public, reproducible payloads representative of large mixed-content
binaries; CAVS itself is payload-agnostic. "Update" means what a client that
already has the previous version
downloads; the baseline is downloading the full new release compressed with
zstd -3.

## Real builds — cold install (dual delivery route)

First install used to be CAVS's weak spot: chunk-level compression cost +2–4%
over downloading the whole release as one `.zst`. The **dual route** removes
it: `cavs pack --bootstrap` also emits the full artifact zstd-19-compressed,
the server offers it to cold clients whenever it beats the chunk path, and the
client **seeds its chunk cache from it** — so the next update is incremental
with zero extra downloads.

| Build | PCK v1 | Full zstd-3 | Chunk path (old cold) | **Dual route cold** | vs zstd-3 |
|---|---:|---:|---:|---:|---:|
| godotengine/tps-demo | 569.15 MiB | 247.62 MiB | 251.91 MiB (+1.7%) | **221.42 MiB** | **−10.6%** |
| GDQuest 3D third-person | 61.09 MiB | 27.66 MiB | 28.20 MiB (+2.0%) | **24.43 MiB** | **−11.7%** |
| MechanicalFlower/Marble | 9.59 MiB | 6.55 MiB | 6.68 MiB (+2.0%) | **5.68 MiB** | **−13.2%** |
| Godot 4.7 export suite | 5.39 MiB | 4.52 MiB | 4.71 MiB (+4.1%) | **4.20 MiB** | **−7.1%** |

The server routed all four builds to the bootstrap automatically (its per-
session estimate vs the 2% threshold), an incompressible payload correctly
stays on the chunk path, and every reconstruction was byte-identical.

## Real builds — update payload

| Build | Update | Full (zstd) | CAVS (64 KiB) | Saved |
|---|---|---:|---:|---:|
| godotengine/tps-demo (569 MB) | tag 4.5 → master (7 files) | 247.60 MiB | **1.64 MiB** | **−99.3%** |
| MechanicalFlower/Marble | 1.6.0 → 1.6.1 | 6.55 MiB | 0.14 MiB | **−97.9%** |
| GDQuest 3D third-person | HEAD~10 → HEAD (468 files) | 27.61 MiB | 8.70 MiB | **−68.5%** |

- Re-downloading the same version is **0 bytes** of payload (everything
  resolves from the persistent cache as references).
- All reconstructions were **byte-identical** (SHA-256 verified), and the Godot
  runtime mounted the reconstructed PCKs via `load_resource_pack()`.
- A client that cold-installed via the bootstrap route pays the same update
  prices: cache seeding reproduces the exact chunk plan of the served version.

## Compact manifest (v0.3.0) — metadata overhead

The runtime manifest used to travel as JSON. The binary v2 format (`CAVSMF2`,
served by content negotiation, JSON kept for compatibility) stores each unique
chunk hash once and references it by varint index. Measured on the same real
builds (64 KiB CDC, `cavs manifest bench` + real HTTP sessions):

| Build | Manifest JSON v1 | Binary v2 | Saved | Parse v1 → v2 |
|---|---:|---:|---:|---|
| godotengine/tps-demo | 894.2 KiB | **208.5 KiB** | **−76.7%** | 0.53 → 0.49 ms |
| GDQuest 3D third-person | 103.1 KiB | **24.8 KiB** | **−75.9%** | 0.062 → 0.058 ms |
| MechanicalFlower/Marble | 20.3 KiB | **5.1 KiB** | **−75.0%** | 0.018 → 0.017 ms |

Chunk-path bytes are unchanged (the tps-demo update is still 1.64 MiB), so the
improvement lands where metadata dominates: a warm re-fetch — the "is there an
update?" check every launcher does — now costs ~75% less wire, and total
update egress improves up to −26.6% (tps-demo: manifest was a third of the
update cost). Cold installs, warm re-fetch = 0 payload bytes and byte-identical
reconstruction were re-verified on all three builds.

## CAVS vs dedicated delta tools

Update payload v1→v2, in MiB. Per-pair deltas win on raw bytes — that is not
the point; the point is operational cost.

| Build | zstd full | zip full | rsync wire | rdiff | xdelta3 -9 | bsdiff | **CAVS (64k)** |
|---|---:|---:|---:|---:|---:|---:|---:|
| tps-demo | 247.60 | 247.51 | 462.95 | 0.70 | 0.03 | 0.03 | **1.64** |
| GDQuest | 27.61 | 27.42 | 54.86 | 7.06 | 3.78 | 3.82 | **8.70** |
| Marble | 6.55 | 6.34 | 0.02 | 0.01 | 0.00 | 0.00 | **0.14** |

Reading the table:

- **rsync loses on large binaries with scattered changes**: for tps-demo it
  transmitted 462.95 MiB — *worse than downloading the whole thing*. Block
  checksums aren't built for this.
- **xdelta3/bsdiff produce the smallest patches** for a single version pair —
  and generating the bsdiff patch for tps-demo cost **137 s and 9.1 GB of
  RAM**. Serving *every* old→new jump as one direct patch needs an all-pairs
  graph (O(N²) patches); practical systems use adjacent diffs, sparse
  ladders, base-version or hot-pair policies instead, each trading storage
  for chain length — the [patch policy benchmark](#pairwise-patch-policy-benchmark)
  measures those directly. CAVS packs **once per release (3.5 s)** and the
  same chunk store serves any version jump, with resumable, CDN-cacheable,
  cross-version reuse.

## Parameter sweeps

Chunk size is the strongest lever. Dropping the FastCDC average from 256 KiB to
64 KiB cut the tps-demo update from 4.97 → **1.64 MiB** (3×) for +1.3% storage.
zstd level 3 was the sweet spot (level 5 saved ~1.5% more cold egress for +33%
packing time). These results set the current defaults: **FastCDC 64 KiB +
zstd 3**.

Since 0.1.2 the sweep is built in: `cavs sweep new.pck --prev old.cavs`
measures eight candidate profiles (fixed 256K/512K/1M, FastCDC
16K/32K/64K/128K/256K) on
the real bytes — chunk counts, sampled compression, manifest weight and real
chunk reuse — and `cavs pack --profile auto` applies the cheapest. It catches
per-title cases a fixed default cannot: for Marble (an in-place patch with no
byte shifting) `fixed-256k` beats `fastcdc-64k` on update egress, 72 vs
153 KiB. Passing `--prev` the *published* `.cavs` keeps the choice consistent
across a version stream, which is what preserves chunk reuse.

## Client cost (tps-demo update, 569 MB, release binaries)

| Metric | Before | After (streaming) |
|---|---:|---:|
| Peak client RAM | 1124 MiB | **7 MiB** |
| Update CPU | 7.0 s | **2.0 s** |
| Pack time (whole release) | 40 s | **3.5 s** |

The client reconstructs by streaming to disk (batch decoded from the socket →
disk cache → `.part` → SHA-256 → atomic rename), so RAM is constant regardless
of payload size.

## Global store — storage dedup at rest

Ingesting two versions of a real build (Marble 1.6.0 and 1.6.1) into the global
content-addressable store stored the shared chunks once: **13.80 MiB logical →
7.04 MiB on disk = ~49% less storage** than keeping each `.cavs` separately,
while serving each version byte-identically over HTTP. Garbage collection
reclaims chunks that no published version references.

## Packfile storage (v0.4.0) — operational shape at rest

`store add --storage packfiles` keeps the same chunks in a few immutable
content-addressed `.cavspack` files instead of one file per chunk, and the
server coalesces each batch's pack reads (nearby chunks = one physical read).
Same two-version stores as above, loose vs packfiles, full
cold + update + warm session per layout:

| Build | Chunk objects on disk | Physical reads (whole session) | Read amplification |
|---|---|---|---|
| MechanicalFlower/Marble | 130 → **4** | 130 → **2** (65×) | 1.000 |
| GDQuest 3D third-person | 807 → **4** | 805 → **7** (115×) | 1.000 |
| godotengine/tps-demo | 5,775 → **6** | 5,775 → **34** (170×) | 1.000 |

Amplification 1.000 means coalescing read **zero** extra bytes: chunks are
written in reconstruction order, so merged ranges are exactly contiguous.
Wire bytes, routing and byte-identical reconstruction are unchanged vs the
loose layout (and vs 0.3.0) — the win is operational: fewer objects to
store/upload/list, fewer syscalls to serve, CDN-ready immutable packs
(`store export` emits the deterministic object tree). Store disk size also
drops slightly (tps-demo: −3.9%, filesystem block overhead of thousands of
small files).

## Hardening (v0.5.0) — recovery, measured

v0.5.0 changes no wire format and no routing: the cold/update/warm numbers
above were re-measured after it and are byte-for-byte identical (tps-demo
update still 1.64 MiB inline, warm still 0 bytes, all reconstructions
byte-identical). What it adds is resilience, and that was measured too:

- **Interrupted install resume** (tps-demo, 232 MiB bootstrap artifact):
  the client was killed with `kill -9` at 57 MiB downloaded. The next
  fetch found the journal, hashed the partial file, continued with an HTTP
  `Range` request and downloaded only the remaining **166.5 MiB** — final
  file byte-identical, journal and partials cleaned up. A stale journal
  (asset republished) is discarded and the fetch starts clean.
- **Cache self-repair** (real cache, 5,747 chunks / 510 MiB): 3 entries
  were corrupted on disk. `cache verify` detected and quarantined exactly
  those 3 (`CAVS-E-CACHE-CORRUPT-RECOVERABLE`), `cache repair` re-fetched
  exactly those 3, and the following re-fetch was back to **0 payload
  bytes** and byte-identical.
- **Corruption matrix**: `cavs test corrupt` runs ~20 targeted mutations
  (container magic/sections/data/truncation, manifest header/body/
  truncation, overlong varints, bootstrap sidecar, packfile header/data/
  footer/index, out-of-range reads) against real containers — every
  corrupted artifact is rejected cleanly on all three builds. The same
  invariants are fuzzed (5 libFuzzer targets) and replayed
  deterministically in CI: full byte-flip sweeps over the pack index and
  container leave **zero** unauthenticated-content survivors.
- **Client memory** (release build, 569 MB payload): peak RSS **14.3 MiB**
  for the cold bootstrap install and **6.3 MiB** for the update — still
  ~constant with asset size.

## Synthetic large builds (v0.5.0) — reproducible suite

`cavs bench gen` emits a deterministic dataset (same seed ⇒ identical bytes
on any machine): a base build plus the update shapes that matter for chunked
delivery. `cavs bench suite` packs and measures every version. 1 GiB base,
FastCDC 64 KiB + zstd 3, Apple Silicon laptop:

| Update shape | Changed | Pack | Update egress |
|---|---|---:|---:|
| v2-small | ~3% of blocks | 6.4 s | 51.3 MiB (5.0%) |
| v2-medium | ~15% | 6.8 s | 216.6 MiB (21.2%) |
| v2-large | ~50% | 6.8 s | 449.1 MiB (43.9%) |
| v2-shifted | 4 KiB inserted at head — **every byte shifts** | 7.3 s | **10.9 KiB (0.0%)** |
| v2-reordered | same blocks, 8 MiB groups swapped | 9.1 s | 20.8 MiB (2.0%) |

The `v2-shifted` row is the reason content-defined chunking exists: a
byte-offset shift that would invalidate every fixed block costs effectively
nothing. Update egress tracks the changed fraction linearly, and manifests
stay compact (1.28 MiB JSON → 311 KiB binary v2 for ~8,600 chunks).
Ingesting v1 + v2-small into a packfile store yields 6 immutable packs.

## Hybrid reconstruction (v0.6.0) — previous install as a byte source

128 MiB synthetic suite (`bench gen --size 128MiB --seed 5`), fastcdc-64k,
release binaries, localhost server. "Cold" = empty chunk cache; "cold +
previous" = empty cache but the old build on disk (the situation every
pairwise patcher assumes — and where v0.5 had to pay the full artifact).

| Update variant | v0.5 cold | v0.6 cold + previous install | Reduction | Warm cache (v0.5 = v0.6) |
|---|---:|---:|---:|---:|
| small change | 64.55 MiB | **6.24 MiB** | **−90.3 %** | 6.24 MiB |
| medium change | 64.56 MiB | **26.53 MiB** | **−58.9 %** | 26.53 MiB |
| shifted (every byte moves) | 64.56 MiB | **10.9 KiB** | **−99.98 %** | 10.9 KiB |

- Warm-cache wire bytes are identical to v0.5 — the unified plan executor
  introduces **no regression** on any existing path (152 workspace tests,
  including the full v0.5 e2e suites, pass unchanged).
- Every copied range is BLAKE3-verified before writing; final SHA-256 gate
  unchanged; all outputs byte-identical (`cmp`).
- Range coalescing: shifted variant plans 1,082 chunk ops → 18 contiguous
  reads (60×); small 1,087 → 154 (7×); strict contiguity, read
  amplification 1.0.
- A previous install with a corrupted byte demotes exactly the affected
  range to network (`CAVS-E-PREVIOUS-ARTIFACT-MISMATCH`) and still ends
  byte-identical.
- No-op re-fetch of an already-current install: **0 payload bytes**,
  ~0.4 s (manifest + one local SHA-256).
- `.cavssig` signature of the 128 MiB build: 88 KiB (0.067 %),
  deterministic, one flipped source byte detected.

## CAVS vs delta patching — v0.6.0 baseline

Block-based delta model (fixed 64 KiB blocks, weak rolling hash + BLAKE3
confirmation, COPY/DATA planning with coalescing, zstd-1 transport), plus
`xdelta3 -9` and `bsdiff` as byte-level baselines, same suite:

| Pair | Full re-download | Block-delta patch | xdelta3 -9 | bsdiff | CAVS update |
|---|---:|---:|---:|---:|---:|
| small | 64.05 MiB | 1.94 MiB | 1.94 MiB | 1.96 MiB | 6.06 MiB |
| medium | 64.05 MiB | 8.83 MiB | 8.82 MiB | 8.90 MiB | 26.28 MiB |
| shifted | 64.06 MiB | 4.04 KiB | 4.65 KiB | 4.67 KiB | 10.90 KiB |

Pairwise patches win per-pair bytes (inherent: they ship dirty regions,
CAVS ships whole chunks); CAVS packages once per release, serves every
version jump from one deduplicated, CDN-cacheable store, and resumes and
self-repairs. Full tables, timings and framing:
[DELTA_COMPARISON.md](DELTA_COMPARISON.md). Compression cross-check
(`bench compression`): zstd and Brotli within 0.1 % on size here, zstd
~40× faster decode — zstd-3 stays the default.

## Offline toolkit & multi-route comparison (v0.7.0)

v0.7.0 adds a local toolkit (`preview`/`diff-plan`/`apply`/`verify-install`)
and benchmark harnesses that put every delivery route in one table for the
same transition. Numbers below are from `cavs bench routes` on the 128 MiB
synthetic builds (seed 5), Apple Silicon, **butler v15.27.0**, xdelta3 3.2.0,
system bsdiff. Every route ended byte-identical (`cmp`/BLAKE3).

### Directory build, typical release (125.8 → 126.9 MiB)

| Route | Network bytes | Diff | Apply | Peak RSS |
|---|---:|---:|---:|---:|
| full download (raw) | 126.89 MiB | — | 0 ms | — |
| full zstd-19 (bootstrap) | 62.12 MiB | 3.9 s | 14 ms | — |
| CAVS chunk / hybrid (wire) | 5.42 MiB | 301 ms | — | — |
| **CAVS offline plan (`.cavsplan`)** | **2.51 MiB** | **488 ms** | **262 ms** | streaming |
| butler offline/default patch | 2.52 MiB (+62 KiB sig) | 983 ms | 348 ms | 35 MiB |
| pairwise proxy: bsdiff+zstd-19 | 3.02 MiB | 25.0 s | 3.4 s | 2.3 GiB |
| pairwise proxy: xdelta3+zstd-19 | 3.01 MiB | 4.3 s | 3.7 s | 397 MiB |

The offline plan is **half** the v0.6 chunk-route wire (one zstd-19 payload
stream instead of per-chunk compression), ties butler on bytes, diffs 2×
faster, and applies with a streaming ~8 MiB budget rather than butler's 35 MiB
or bsdiff's 2.3 GiB.

### Single 128 MiB artifact

| Pair | full zstd-19 | CAVS chunk/hybrid | **CAVS plan** | butler offline | bsdiff proxy | xdelta3 proxy |
|---|---:|---:|---:|---:|---:|---:|
| small change (~3%) | 64.05 MiB | 6.06 MiB | **1.94 MiB** | 1.94 MiB | 1.96 MiB | 1.94 MiB |
| shifted (every byte moves) | 64.06 MiB | 10.90 KiB | **4.21 KiB** | 68.13 KiB | 4.59 KiB | 4.34 KiB |

Four tools land within 1 % on the small change — the changed bytes are the
floor; the difference is time, memory and delivery model. On the shifted
artifact CAVS's content-defined blocks survive the unaligned insert and match
the byte-level tools, beating butler's fixed-block scan **16×**.

### Directory vs one compressed blob (same content change, 62 MiB)

| Shape | CAVS offline plan | butler offline | xdelta3 proxy |
|---|---:|---:|---:|
| directory build | **2.51 MiB** | 2.52 MiB | 3.01 MiB |
| same build as one zstd blob | 21.92 MiB | 21.92 MiB | 2.53 MiB |

Block-level delivery of a compressed archive costs **~9×** more than the same
change in directory mode — the compression cascades one edit across the whole
output. `cavs preview` warns about this shape; publish folders, not archives.

### Many-version stream (10 versions × 32 MiB, ~3 % drift/release)

| Method | Storage | Adjacent updates | v1→v10 jump | Any-pair coverage |
|---|---:|---:|---:|---|
| CAVS packfile store | **30.60 MiB** (10 packs) | 13.70 MiB total | 8.95 MiB | every pair, same objects |
| bsdiff patches | 4.23 MiB (9 adjacent) + full artifacts | 4.23 MiB total | 3.60 MiB (dedicated) | all-pairs one-hop needs 45 patches; practical policies chain or budget instead |

Per-pair, bsdiff is smaller — expected and fine. The store-once model wins on
the operational axis: ten versions fit in less space than one raw build, and
*any* jump (v1→v10, v3→v10, reinstall) is served from the same immutable
objects with zero per-pair generation.

The v0.7.0 butler numbers are its **default** patch; bsdiff/xdelta3 are
labeled optimized pairwise **proxies**. v0.8.0 measures butler's
optimized patch directly — see the next section. Full framing:
[BUTLER_COMPARISON.md](BUTLER_COMPARISON.md),
[ROUTE_BENCHMARKS.md](ROUTE_BENCHMARKS.md).

## Full-pipeline & delivery planner (v0.8.0)

v0.8.0 adds the delivery planner (`cavs route-plan`), per-file optimized
sidecars (`.cavspatch` v2) and `cavs bench full-pipeline`, which measures
every CAVS route and butler's **complete** pipeline — default `diff` and
optimized `rediff --rediff-quality 9` — on the same transition (butler
v15.28.0; CAVS apply times/RSS measured via real subprocesses under
`/usr/bin/time`; all outputs byte-identical).

**A — typical directory release (126 MiB):** CAVS auto-route (plan)
2.51 MiB / gen 0.54 s / apply 391 ms / 23 MiB RSS — vs butler optimized
2.51 MiB / 11.6 s / 403 ms / 97 MiB. Bytes and apply tie; **4.2× less
memory, 21× faster generation**.

**B — shifted artifact (128 MiB, 4 KiB head insert):** CAVS 4.21 KiB /
170 ms / 11 MiB vs butler optimized 11.39 KiB / 370 ms / 94 MiB — CAVS
wins every column, and beats raw bsdiff (4.59 KiB) and xdelta3
(4.34 KiB) on bytes.

**C — compressed blob (62 MiB .tar.zst):** the v0.7.0 weak case is
closed. The per-file strategy optimizer routes the high-entropy blob
through xdelta3: CAVS auto-route **2.53 MiB** (was 21.9 MiB through
block routes), 13% below butler optimized (2.90 MiB).

**D — many-version storage (10 × 32 MiB):** CAVS store + 3 policy-chosen
hot-pair sidecars = **35.91 MiB** serving any jump; all-pairs one-hop bsdiff
coverage = 144.23 MiB in 45 patches — **75% less storage**. (All-pairs is
the theoretical one-hop baseline, not how pairwise systems normally deploy;
v1.1.0's patch policy benchmark compares the practical policies too.)

**E — low-memory apply (256 MiB build):** a bsdiff sidecar applies with
**517 MiB real RSS**; under `--memory-budget 128MiB` CAVS refuses it up
front (`CAVS-E-MEMORY-BUDGET-EXCEEDED`) and the planner serves the plan
route instead: 7.63 MiB (0.5% *smaller* than the sidecar) at **27 MiB
real RSS**.

**F — interrupted apply:** `cavs test apply-recovery` — 10 SIGKILLed
applies recovered, corrupt plan rejected untouched, corrupted old
install self-healed via deduplicated content (output verified), garbage
staging re-staged. Mods and mtimes survive every case.

Full tables: [ROUTE_BENCHMARKS.md](ROUTE_BENCHMARKS.md); planner:
[DELIVERY_PLANNER.md](DELIVERY_PLANNER.md); sidecar format and policy:
[PAIRWISE_SIDECARS.md](PAIRWISE_SIDECARS.md).

## SteamPipe-style analysis (v0.9.0)

v0.9.0 turns CAVS into a build-update lab: a SteamPipe-style
fixed-1MiB update model (`cavs bench steampipe-style`), the layout
analyzer (`cavs analyze steampipe`, `analyze-packs`), publish previews,
a local disk I/O estimator, a policy route planner and the local
app/depot/branch/build workspace. Every SteamPipe-style figure is a
public-model **estimate**, never Valve's implementation
([STEAMPIPE_STYLE_MODEL.md](STEAMPIPE_STYLE_MODEL.md)). Raw outputs:
[results/v0.9.0/](results/v0.9.0/).

**A/B — pack pathology (32 × ~1 MiB assets):** the same 64 KiB of real
change costs **1.00 MiB** (localized), **1.88 MiB** (TOC at the end) or
**the whole 32.88 MiB pack** (shifted / shuffled / distributed-TOC)
under the fixed model. The CAVS `.cavsplan` for the shifted pack is
**7.4 KiB** — content-defined chunking is immune to offset cascades,
with no per-pair patch. The analyzer diagnoses each case
(`asset_shuffling`, `toc_churn`) and its fix; applying the fixes
(centralized TOC, padded per-asset compression) recovers 94% / 75%
fixed reuse.

**C — directory vs blob:** the same assets as individual files cost
1.00 MiB — layout-equivalent to the best pack, with per-file staged
applies.

**D — depot sharing:** windows ↔ linux depots share **98.9%** of their
bytes; install plans price ownership/platform/language per depot, and a
demo owner with the full build installed downloads **0 B**
(cross-depot chunk reuse).

**E — many-version stream (10 × 24 MiB):** the content-addressed store
holds all 10 versions in **22.43 MiB** and serves any jump
(v1→v10: 6.58 MiB) with no extra server work; direct pairwise coverage
would need 45 patches.

**F — local disk I/O:** a ~3-byte change in a 256 MiB pack downloads
2 MiB but costs **512 MiB of local I/O** — on an HDD (4.7 s) that is
*slower* than the raw 256 MiB full download (2.6 s). Splitting the pack
into 8 parts cuts it to 128 MiB (1.2 s). `cavs io-estimate` flags when
I/O dominates the network saving.

**G — Godot PCK:** one edited resource costs 1.00 MiB (model) vs
128 KiB (CAVS plan); a resource packed in front shifts everything —
3.50 MiB vs 1.06 MiB — and `cavs analyze godot-pck` names the exact
`res://` paths behind the churn.

**H — route planner:** `cavs plan-update` picks `.cavsplan` (129 KiB)
for every previous-install state and honestly picks the raw full
download for a cold install of incompressible data (a bootstrap would
save nothing). Unavailable routes are never chosen.

Summary tables: [STEAMPIPE_COMPARISON.md](STEAMPIPE_COMPARISON.md);
analyzer guide: [BUILD_UPDATE_ANALYZER.md](BUILD_UPDATE_ANALYZER.md);
layout rules: [PACK_FILE_OPTIMIZATION.md](PACK_FILE_OPTIMIZATION.md).

## Pairwise patch policy benchmark (v1.1.0)

Earlier versions of this document described all-pairs pairwise diffs as
O(N²). That is true for a theoretical one-hop patch graph, but practical
systems usually use adjacent diffs, sparse ladders, base-version
policies, or selected hot pairs. This section compares CAVS against
those policies directly: `cavs bench patch-policy` on the deterministic
10-version stream (`cavs bench gen-stream`, 32 MiB per version, ~3%
drift per release), every pairwise number a real diff, applied and
byte-verified. Full harness: [PATCH_POLICY_BENCHMARK.md](PATCH_POLICY_BENCHMARK.md).

### Policies tested

| Policy | Patch count | Best use case |
|---|---:|---|
| Adjacent | N−1 | users update every version |
| Ladder | <2N | skipped versions, bounded chains |
| Base hub | 2(N−1) | major baseline workflows |
| Hot pairs | budgeted | traffic-driven optimization |
| All-pairs | N² | theoretical one-hop lower bound |
| CAVS | content store | route-planned cache/hybrid updates |

### Results — `adjacent-heavy` traffic (80% adjacent, 15% skip 2–4, 4% old→latest, 1% reinstall; cold cache + previous install)

| Policy | Patches | Storage | Avg update | P95 update | P99 update | Max steps | Build time |
|---|---:|---:|---:|---:|---:|---:|---:|
| Adjacent | 9 | **4.20 MiB** | 891 KiB | 2.00 MiB | 4.20 MiB | 9 | 0.9 s |
| Ladder (aligned) | 16 | 14.59 MiB | 877 KiB | 1.88 MiB | 3.76 MiB | 4 | 2.0 s |
| Base hub (v06, auto) | 18 | 21.41 MiB | 2.12 MiB | 3.76 MiB | 3.88 MiB | 2 | 2.6 s |
| Hot pairs (latest:3, 2×-build budget) | 11 | 6.39 MiB | 888 KiB | 2.00 MiB | 4.13 MiB | 7 | 1.1 s |
| All-pairs (theoretical one-hop) | 45 | 70.49 MiB | **867 KiB** | **1.82 MiB** | **3.57 MiB** | **1** | 8.2 s |
| CAVS | content store | 29.84 MiB | 2.26 MiB | 5.12 MiB | 8.95 MiB | **1** | **0.4 s** |

### Results — `skip-heavy` traffic (40% adjacent, 40% skip 2–8, 15% old→latest, 5% reinstall; bsdiff engine)

| Policy | Storage | Avg update | P95 update | P99 update | Max steps | Build time |
|---|---:|---:|---:|---:|---:|---:|
| Adjacent | **4.23 MiB** | 2.28 MiB | 4.23 MiB | 16.02 MiB | 9 | 56 s |
| Ladder (aligned) | 14.71 MiB | 2.22 MiB | 3.79 MiB | 16.02 MiB | 5 | 118 s |
| Base hub (v06) | 21.59 MiB | 2.94 MiB | 3.91 MiB | 16.02 MiB | 2 | 167 s |
| Hot pairs (latest:3) | 6.44 MiB | 2.27 MiB | 4.17 MiB | 16.02 MiB | 8 | 72 s |
| All-pairs (theoretical one-hop) | 71.07 MiB | **2.17 MiB** | **3.60 MiB** | 16.02 MiB | 2 | 473 s |
| CAVS | 29.84 MiB | 4.60 MiB | 8.95 MiB | 16.15 MiB | **1** | **0.4 s** |

The P99 tail under skip-heavy is ~16 MiB for every policy — the shared
compressed full-download cost of the 5% reinstalls, which no pairwise
policy patches and CAVS serves from its store. It is the same for
everyone because it is a property of the traffic, not the policy.

### Results — `adjacent-heavy` traffic, `warm-cache` client state

Same graph, warm chunk cache (the client's cache accumulated the chunks
of every version it already passed through). Only the CAVS route
changes — pairwise patches don't depend on cache state — and it doesn't
help much here because each release introduces genuinely new chunks;
the win from a warm cache is on the download tail, not the average.

| Policy | Avg update | P95 update | P99 update | Max steps |
|---|---:|---:|---:|---:|
| Adjacent | 898 KiB | 2.02 MiB | 4.23 MiB | 9 |
| Ladder (aligned) | 883 KiB | 1.89 MiB | 3.79 MiB | 5 |
| All-pairs (theoretical one-hop) | **872 KiB** | **1.83 MiB** | **3.60 MiB** | **1** |
| CAVS (warm) | 2.26 MiB | 5.12 MiB | 8.95 MiB | **1** |

### Per-query examples (cavsplan engine)

| From | To | Adjacent | Ladder | Base | All-pairs | CAVS |
|---|---|---:|---:|---:|---:|---:|
| v01 | v02 | 513 KiB / 1 step | 513 KiB / 1 | 3.76 MiB / 2 | 513 KiB / 1 | 1.69 MiB / 1 |
| v01 | v10 | 4.20 MiB / 9 steps | 3.76 MiB / 2 | 3.88 MiB / 2 | 3.57 MiB / 1 | 8.95 MiB / 1 |

Reading the numbers honestly:

- **Adjacent diffs win storage and per-update bytes for users who update
  every release** — that is their design point, and the benchmark
  confirms it. The cost is chains: 9 sequential applies for v01→v10,
  every intermediate patch must exist and apply cleanly.
- **The ladder is the strongest practical pairwise baseline here**: near
  adjacent-level bytes with chains bounded at 4–5 steps for 3.5× the
  storage.
- **All-pairs is the byte/steps optimum and the storage/build
  pathology** — 70 MiB of patches and 455 s of bsdiff build time for ten
  32 MiB versions. It stays in the table as the labeled theoretical
  baseline, not as "pairwise diffs".
- **CAVS trades per-pair bytes for operational properties**: one apply
  step for any jump, 0.4 s of build (pack once, no per-pair work),
  reinstalls served from the same store, cache/hybrid reuse, and no
  patch graph to host or expire. Under skip-heavy traffic its worst case
  (8.95 MiB, the v01→v10 cold jump) is bounded by the store — a client
  never chains.
- Exact pairwise patches on this dataset are ~2–4× smaller than the
  CAVS chunk route for the same pair (e.g. 3.57 vs 8.95 MiB for
  v01→v10) — consistent with every earlier per-pair comparison in this
  document. Policy-level costs (chains, storage, build, coverage) are
  the context those per-pair numbers were missing.

Raw reports (summary, per-edge CSV, per-query CSV, storage/traffic/
apply-chain reports, replayable `patch_graph.json`):
[results/v1.1.0/patch-policy/](results/v1.1.0/patch-policy/). Replay a
different traffic model or client state without re-diffing:
`cavs patch-policy simulate --graph …/patch_graph.json --traffic-model
major-release`.

## Small chunk profiles, tight normalization & parallel pack (v1.3.0)

Measured on two real version pairs — the same Marble PCK pair used above
(official 1.6.0 → 1.6.1 Linux release builds) and the Godot editor 4.2 →
4.2.1 linux binary (100 MB) — plus the 256 MiB synthetic suite. All
reconstructions byte-identical (`cmp`).

**Update egress, Marble PCK pair** (what a client holding 1.6.0 downloads):

| Config | Update wire | Cold wire | Manifest v2 |
|---|---:|---:|---:|
| fastcdc-64k + zstd-3 (previous default) | 1.500 MiB | 7.81 MiB | ~4.9 KiB |
| fastcdc-32k + zstd-3 | 0.798 MiB (**−47%**) | 8.01 MiB | ~11 KiB |
| fastcdc-16k + zstd-3 | 0.525 MiB (**−65%**) | 8.12 MiB | ~22 KiB |
| fastcdc-16k + zstd-19 | **0.476 MiB (−68%)** | 7.50 MiB (−4%) | ~22 KiB |

The +4% cold chunk-path cost of 16 KiB chunks is recovered by zstd-19 —
and the dual bootstrap route serves cold installs anyway. The synthetic
suite confirms the trend (v2-small update 11.89 → 8.56 MiB, −28%) and the
shifted case stays at its floor (10.9 KiB).

**Why the sweep missed this before:** the auto-profile cost model priced
manifests at the JSON-era 150 B/chunk; the binary v2 manifest measures
~36 B/chunk, so small-chunk profiles were penalized ~4× too hard. Fixed,
and `fastcdc-16k`/`fastcdc-32k` joined the candidate set. The new
profiles use FastCDC normalization level 3 (tight chunk-size spread:
p10–p90 narrows from ~30–140 KiB to ~65–85 KiB at a 64 KiB average),
worth ~−20% additional update egress at the same average; the existing
profiles keep their published boundaries bit-for-bit (compatibility test
pinned).

**Parallel pack:** per-chunk BLAKE3 + zstd now run on all cores, with
ingest order and output file byte-identical to the serial path (test
pinned). Measured on the 100 MB Godot binary: 323 → 1,022 MiB/s at
zstd-3 (**3.2×**), 8 → 54 MiB/s at zstd-19 (**7.2×**). That makes
per-chunk **zstd-19 practical for release publishing**: −9/10% wire and
storage on both real pairs with identical client decode cost and
unchanged server passthrough (chunks still travel as stored).

**Measured and rejected:** per-release zstd dictionaries (−0.5–3.5%, not
worth the format change) and bootstrap long-distance matching / level 22
(−0.03–1.15%; the zstd-19 bootstrap is already near-optimal).

## Honest negatives (video suite)

CAVS is not a codec and doesn't pretend to be:

- **Single video, first play**: ~0% savings, packaging overhead +0.03%.
- **ABR ladder** (same title, multiple bitrates): ~0% cross-bitrate dedup —
  different bitstreams share no bytes. Expected and reported.
- **Already-compressed files** (JPEG, MP4, ZIP): overhead +0.08%, near-zero
  savings — the value is elsewhere. The payload classifier now detects these
  (entropy + zstd probe) and packs them with large fixed chunks so the
  overhead stays minimal instead of paying for useless small chunks.
- Where redundancy *does* cross content (shared episode intros): 7–21% storage
  savings by codec; warm re-watch: −100% egress.

## Reproducing

The synthetic large-build suite is fully reproducible from this tree:
`cavs bench gen --out ds --size 1GiB && cavs bench suite --dataset ds --out
results` regenerates the exact same dataset (deterministic PRNG) and writes
`summary.md`/`summary.json`. The patch policy benchmark reproduces the same
way: `cavs bench gen-stream --out builds --versions 10 --size 32MiB` then
`cavs bench patch-policy --versions-dir builds --version-glob 'v*'
--traffic-model adjacent-heavy --hot-pairs latest:3 --patch-storage-budget
2x-latest-build --out results/patch-policy` (bsdiff/xdelta3 columns appear
when the tools are installed; missing tools are skipped, never fatal). The real-payload harnesses (`cavs-bench`, the
corpus scripts) and their raw result data live in the full development
repository, not in this open-source tree; the measured summaries above are
what those harnesses produced.
