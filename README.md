# CAVS — Content-Addressable Verified Streaming

[![CI](https://github.com/orelvis15/cavs-oss/actions/workflows/ci.yml/badge.svg)](https://github.com/orelvis15/cavs-oss/actions/workflows/ci.yml)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

**Latest versions** — each product releases on its own independent version train (badges track the latest published release automatically):

[![core + CLI on crates.io](https://img.shields.io/crates/v/cavs-cli?label=core%20%2B%20cli&color=1f6feb)](https://crates.io/crates/cavs-cli)
[![SDK on npm](https://img.shields.io/npm/v/%40orelvis15%2Fcavs-sdk?label=sdk%20%C2%B7%20npm)](https://www.npmjs.com/package/@orelvis15/cavs-sdk)
[![SDK on Maven Central](https://img.shields.io/maven-central/v/io.github.orelvis15/cavs-sdk?label=sdk%20%C2%B7%20maven)](https://central.sonatype.com/artifact/io.github.orelvis15/cavs-sdk)
[![Godot plugin](https://img.shields.io/github/v/release/orelvis15/cavs?filter=plugins-*&label=godot%20plugin&color=2ea043)](https://github.com/orelvis15/cavs/releases?q=plugins)
[![Desktop app](https://img.shields.io/github/v/release/orelvis15/cavs?filter=desktop-*&label=desktop&color=2ea043)](https://github.com/orelvis15/cavs/releases?q=desktop)

**Get it** — **Core / CLI:** `cargo install cavs-cli` · **SDKs:** [npm](https://www.npmjs.com/package/@orelvis15/cavs-sdk) · [Maven Central](https://central.sonatype.com/artifact/io.github.orelvis15/cavs-sdk) · [pkg.go.dev](https://pkg.go.dev/github.com/orelvis15/cavs-oss/sdks/go) · **Godot plugin:** [download](https://github.com/orelvis15/cavs/releases?q=plugins) · **Desktop (Windows / macOS / Linux):** [download](https://github.com/orelvis15/cavs/releases?q=desktop)

The core and the SDKs share a version (the SDKs bind the core through a C ABI); the engine plugins and the desktop app version independently. See [RELEASING.md](docs/RELEASING.md) for how the release trains work.

**Ship updates that weigh what *changed*, not what the payload weighs.**

CAVS is a content-addressable, verified delivery layer for **large files** —
application and machine builds, AI models and checkpoints, datasets, disk and
container images, media bundles, binary archives, backups, patches. It sits
**on top of** your existing formats (it doesn't replace them) and makes every
client download only the chunks it doesn't already have. It is a transport and
dedup layer, not a compressor or a pixel codec: bring the best format for your
bytes, CAVS moves and stores them efficiently.

- **Content-addressable**: every chunk is identified by its BLAKE3-256 hash;
  the client fetches only what it lacks, with a cache that reuses bytes across
  versions, add-on content and sessions.
- **Cold installs at less than full-download price (dual route)**: packing
  with `--bootstrap` also emits the whole release as one zstd-19 artifact;
  the server routes cache-less clients to it automatically whenever it beats
  the chunk path, and the client **seeds its chunk cache from it** — so the
  first install is cheap *and* the next update is already incremental.
- **Adaptive chunking**: `--profile auto` classifies the payload (format
  magic, sampled entropy, compression probe) and measures candidate chunk
  profiles on the real bytes; `cavs sweep` prints the per-payload table.
- **Verified end-to-end**: per-chunk BLAKE3, global Merkle root, per-file
  SHA-256, and an optional Ed25519 content signature. Reconstruction is
  byte-identical or it fails — never halfway.
- **Constant memory**: the client reconstructs by streaming to disk
  (`.part` → verify → atomic rename), so RAM stays ~constant regardless of
  payload size.
- **Compact manifests (v0.3.0)**: the runtime manifest travels as a compact
  binary format (`CAVSMF2`) — ~75–77% smaller than the JSON equivalent on real
  builds — negotiated transparently, with JSON kept as the default response,
  debug export and compatibility path for older clients.
- **Packfile storage (v0.4.0)**: the global store can keep its chunks in a
  few immutable, content-addressed `.cavspack` files instead of one file per
  chunk — on a 570 MB build, 5,775 objects become 6 files and an update
  session's reads coalesce 170× with zero read amplification. Exportable as
  a deterministic object tree for S3/R2/CDN.
- **Production-hardened (v0.5.0)**: interrupted downloads resume with HTTP
  Range instead of restarting; transient network failures retry with
  backoff; a corrupt cache is detected, quarantined and repaired in place;
  every decoder is fuzzed and survives full byte-flip/truncation sweeps;
  failures carry stable `CAVS-E-*` error codes; and `cavs doctor` diagnoses
  a deployment in one command.
- **Offline toolkit (v0.7.0)**: sign, preview, diff, apply, verify and
  benchmark builds locally with no server — `cavs preview` /`diff-plan` /
  `apply` / `verify-install` / `file` / `ls`, portable `.cavsplan` patches
  with journaled staged applies, stable directory mode with `.cavsignore`,
  and a fair external **butler offline** benchmark plus a multi-route
  comparison suite.
- **Delivery planner (v0.8.0)**: `cavs route-plan` scores every route
  (no-op / chunks / hybrid / plan / sidecar / bootstrap / full) for one
  concrete client state under device profiles; `.cavspatch` v2 optimized
  sidecars pick the best strategy **per file** (copy-old with rename
  detection, plan ops, bsdiff, xdelta3, full data) by measuring real
  candidates; `cavs patch-policy` keeps sidecars to hot pairs (never
  the all-pairs O(N²) graph); `cavs publish-dir` produces a whole
  release in one pass; and
  `cavs bench full-pipeline` proves it against the complete external
  butler pipeline (default *and* rediff-optimized patches).
- **SteamPipe-style local analysis (v0.9.0)**: measure and fix update
  behavior *before* publishing. `cavs bench steampipe-style` estimates a
  build transition under a public fixed-1MiB model; `cavs analyze
  steampipe` / `analyze-packs` / `analyze godot-pck` diagnose scattered
  churn, asset shuffling, distributed-TOC/offset cascades, compressed
  blobs and metadata churn — with concrete fixes; `cavs publish-preview`
  measures every route and recommends one; `cavs io-estimate` prices the
  local disk I/O per device; `cavs plan-update` scores routes under
  explicit policies; a local app/depot/branch/build **workspace** models
  depots, branches, promotion/rollback, content sharing and install
  plans; `cavs serve` exposes it all to dev clients; and `cavs build
  sign/encrypt` adds release authenticity (not DRM). Estimates are
  SteamPipe-*style* — a public model, never Valve's implementation.
- **Release certification (v1.0.0)**: `cavs certify` answers *"is this
  update ready to publish?"* in one command — mandatory byte-identical
  reconstruction, path safety and corruption smoke checks, route
  selection certified per client state, a measured route matrix, a
  regression guard against a recorded baseline, Godot PCK and
  workspace/install-plan certification, and a deterministic
  reproducibility bundle others can verify. Profiles from `quick` to
  `strict`, stable exit codes and JSON schemas for CI. See
  [docs/CERTIFICATION.md](docs/CERTIFICATION.md) and
  [docs/TRY_CAVS.md](docs/TRY_CAVS.md).
- **Patch policy benchmark (v1.1.0)**: pairwise diffs are not one
  strategy, so CAVS benchmarks the *policies* real systems deploy —
  adjacent-only diffs, sparse power-of-two ladders, base-version hubs,
  hot pairs under a storage budget — against the all-pairs one-hop
  baseline (kept only as the theoretical bound) and against CAVS
  content-addressed routes, under explicit user traffic models
  (`cavs bench patch-policy`, `cavs patch-policy
  graph`/`simulate`/`explain`, `cavs bench gen-stream`). See
  [docs/PATCH_POLICY_BENCHMARK.md](docs/PATCH_POLICY_BENCHMARK.md) and
  [docs/PRACTICAL_PAIRWISE_DIFFS.md](docs/PRACTICAL_PAIRWISE_DIFFS.md).
- **Serverless / CDN-only delivery + embeddable client (v1.4.0)**: `cavs
  store export --static-plans` now emits a self-describing static tree
  (immutable packs + per-asset `manifest.json` + `chunk-map.json`), and
  `cavs-client fetch-static <url|dir> <asset>` installs and updates a build
  **straight from it with no `cavs-server`** — planning locally and pulling
  only changed chunks over concurrent HTTP Range requests. The same engine
  ships as a library (`cavs-fetch`), an SDK operation (`fetchStatic` in Go /
  Kotlin / Node) and through the C ABI, so installers, launchers and
  applications self-update **in-process** with progress and cancellation —
  which is also what the (untested) Unity and Unreal plugins call. Container
  fetches can also opt
  into a **content-addressed parallel download** (`--connections N`): −26%
  wall time at 4 connections on a localhost origin, more on latency-bound
  links, with byte-identical egress. See
  [docs/SERVERLESS_DELIVERY.md](docs/SERVERLESS_DELIVERY.md) and
  [docs/EMBEDDABLE_FETCH.md](docs/EMBEDDABLE_FETCH.md).
- **Git LFS transfer agent (v1.5.0)**: `cavs-lfs-agent` plugs CAVS into Git
  LFS as a standalone custom transfer agent — LFS objects are chunked and
  deduplicated at the remote, so pushing a 22 MiB asset with ~3 MiB of
  changes stores and sends ~3 MiB instead of the whole file, and pulls reuse
  the local chunk cache across versions. Works against a plain directory
  remote (read/write) or a CDN/static host (read-only). See
  [docs/GIT_LFS.md](docs/GIT_LFS.md).
- **Complementary, not competitive**: use the best codec/compressor for the
  bytes; CAVS deduplicates and transports above them.

## Why it matters (measured on real builds)

The test corpus is real-world binary builds: consecutive versions of
open-source Godot projects exported to PCK — chosen because they are public,
reproducible and representative of large mixed-content payloads — served over
real HTTP sessions. "Update" = what a client that already has the previous
version downloads, versus downloading the full new release compressed with zstd.

| Build | Update | Full download | With CAVS | Saved |
|---|---|---:|---:|---:|
| godotengine/**tps-demo** (569 MB) | tag 4.5 → master | 247.6 MiB | **1.64 MiB** | **−99.3%** |
| MechanicalFlower/**Marble** | 1.6.0 → 1.6.1 | 6.55 MiB | 0.14 MiB | **−97.8%** |
| GDQuest **3D third-person** | HEAD~10 → HEAD (468 files) | 27.61 MiB | 8.7 MiB | **−68.5%** |

And the **first install** (a cache-less client) now costs *less* than
downloading the full compressed release, thanks to the dual delivery route:

| Build | Full download (zstd-3) | CAVS cold install | Delta |
|---|---:|---:|---:|
| godotengine/**tps-demo** | 247.62 MiB | **221.42 MiB** | **−10.6%** |
| GDQuest **3D third-person** | 27.66 MiB | **24.43 MiB** | **−11.7%** |
| MechanicalFlower/**Marble** | 6.55 MiB | **5.68 MiB** | **−13.2%** |

- **Re-downloads cost ~0 bytes** of payload (persistent content-addressable cache).
- **Interrupted installs don't start over** (v0.5.0): a 232 MiB install
  killed at 57 MiB resumed with an HTTP Range request and downloaded only
  the missing ~166 MiB — verified end to end, never promoting an
  unverified file.
- **Content shifts don't break updates**: inserting bytes at the head of a
  1 GiB build (every downstream byte moves) costs **10.9 KiB** of update
  egress — FastCDC re-synchronizes (`cavs bench suite`, reproducible).
- **Server storage dedup**: ingesting two versions of a real build into the
  global store stored the shared chunks once — **~49% less disk** than keeping
  each `.cavs` separately.
- **Client RAM is constant at ~7–14 MiB**, whether the payload is 9 MB or
  569 MB.
- **Honest negatives**: on a single video, ABR ladders, or already-compressed
  files, savings are ~0 and the packaging overhead is +0.03–2% (the payload
  classifier keeps it at the low end by using large chunks there).

Full methodology and comparisons vs xdelta3/bsdiff/rdiff/rsync are in
[`docs/BENCHMARKS.md`](docs/BENCHMARKS.md). Design rationale and results are in
the paper, [`docs/PAPER.md`](docs/PAPER.md).

## Repository layout

| Folder | What |
|---|---|
| [`core/`](core) | The delivery engine (Rust): chunking, hashing, the `.cavs` format, the global content-addressable store, the CVSP protocol, the SteamPipe-style analyzer (`cavs-analyzer`), the local workspace model (`cavs-workspace`), the embeddable serverless fetch engine (`cavs-fetch`), the SDK operation engine (`cavs-sdk-core`) and its C ABI (`cavs-ffi`), and the `cavs` / `cavs-server` / `cavs-client` binaries |
| [`sdks/`](sdks) | Language SDKs over the shared Rust core via the C ABI: [Go](sdks/go), [Kotlin/JVM](sdks/kotlin) and [Node/TypeScript](sdks/node) |
| [`game-engine-plugins/`](game-engine-plugins) | One concrete family of host integrations over the shared core, for runtimes that mount content packs at runtime: [Godot 4](game-engine-plugins/godot-plugin) client in pure GDScript (downloads, verifies and mounts packs with `load_resource_pack()`), plus [Unity](game-engine-plugins/unity-plugin) (C# P/Invoke) and [Unreal](game-engine-plugins/unreal-plugin) (C++) clients over the C ABI — **reference integrations, currently untested**. Any other host embeds the same engine via `cavs-fetch`, an SDK or the C ABI |
| [`docs/`](docs) | Format specification, architecture, benchmarks, and the technical paper |

## Getting started

### Prerequisites

- **Rust** (stable) — install via [rustup](https://rustup.rs). No other
  dependency is needed for the generic file/build (`--raw`) path.
- **ffmpeg** on `PATH` — only for the optional video packaging mode.
- **Godot 4** — only if you use the Godot plugin.

### Build

```sh
git clone https://github.com/orelvis15/cavs-oss.git && cd cavs-oss
cargo build --release
```

This produces the binaries in `target/release/`:

- `cavs` — the packaging CLI (including the SteamPipe-style analysis
  commands: `analyze steampipe`, `bench steampipe-style`,
  `publish-preview`, `analyze-packs`, `io-estimate`, `plan-update`)
- `cavs-server` — the origin server
- `cavs-client` — the native client

### Test

```sh
cargo test            # unit + integration + end-to-end tests
cargo clippy --all-targets   # lints
```

### Try it end to end

Package two versions of a build, serve them, and watch a client download only
what changed on the second fetch:

```sh
# 1. Package two versions of a build. --profile auto picks the chunking
#    per payload, --bootstrap makes cold installs cost the full artifact, and
#    --prev keeps the chunk profile consistent with the published version.
./target/release/cavs pack --raw build_v1.bin --profile auto --bootstrap -o build_v1.cavs
./target/release/cavs pack --raw build_v2.bin --profile auto --prev build_v1.cavs --bootstrap -o build_v2.cavs

# 2. Inspect and verify
./target/release/cavs info build_v1.cavs
./target/release/cavs verify build_v1.cavs

# 3. Serve both versions (the .bootstrap.zst sidecars are picked up next to them)
./target/release/cavs-server build_v1.cavs build_v2.cavs --listen 127.0.0.1:8990

# 4. A cold client installs v1 (routed to the bootstrap, cache auto-seeded),
#    then updates to v2 — the second fetch downloads only the changed chunks
./target/release/cavs-client fetch http://127.0.0.1:8990 build_v1 -o out1 --cache ./cache
./target/release/cavs-client fetch http://127.0.0.1:8990 build_v2 -o out2 --cache ./cache

# Optional: measure which chunk profile is cheapest for YOUR builds
./target/release/cavs sweep build_v2.bin --prev build_v1.cavs
```

Before you ship an update, certify it (v1.0.0):

```sh
# Byte-identical reconstruction, route selection per client state,
# regression guard, pack-layout analysis — one report, stable exit codes.
./target/release/cavs certify --old ./Build_v1 --new ./Build_v2 \
  --profile release --out ./certification
cat certification/summary.md
```

Signing (optional, recommended for distribution):

```sh
./target/release/cavs keygen -o publisher.key                     # → publisher.key(.pub)
./target/release/cavs pack --raw build_v2.bin --sign-key publisher.key -o build_v2.cavs
./target/release/cavs-client fetch <url> build_v2 -o out --cache ./cache --pubkey publisher.key.pub
```

### Global content-addressable store (dedup at rest across all versions)

Store each unique chunk once across every version and artifact, with reference
counting and garbage collection. With `--storage packfiles` (v0.4.0) the
chunks live in a few immutable packfiles served by coalesced range reads:

```sh
./target/release/cavs store ./store add build_v1 build_v1.cavs --storage packfiles
./target/release/cavs store ./store add build_v2 build_v2.cavs   # shared chunks stored once
./target/release/cavs store ./store stat                        # storage savings + pack occupancy
./target/release/cavs store ./store verify                      # re-hash chunks, check packs
./target/release/cavs store ./store gc --grace 0                # reclaim unreferenced chunks/packs
./target/release/cavs store ./store export --out ./dist         # immutable tree for S3/R2/CDN
./target/release/cavs-server --store ./store --listen 127.0.0.1:8990
```

### Serverless delivery — update clients with no server (v1.4.0)

Export the store as a self-describing static tree and update clients straight
from it — S3, R2, GitHub Pages, nginx or a local folder — with no
`cavs-server` running:

```sh
# Publish: a static tree (packs + manifest.json + chunk-map.json)
./target/release/cavs store ./store export --out ./dist --static-plans
# (upload ./dist to any static host that honours HTTP Range)

# Install / update a client from the tree; only changed chunks travel,
# downloaded concurrently and verified end to end.
./target/release/cavs-client fetch-static https://cdn.example.com/dist build_v2 \
  -o ./install --cache ./cache --connections 8
```

The same engine is a library (`cavs-fetch`), an SDK op (`fetchStatic`) and a C
ABI call, so an installer, launcher or application self-updates in-process — see
[docs/SERVERLESS_DELIVERY.md](docs/SERVERLESS_DELIVERY.md) and
[docs/EMBEDDABLE_FETCH.md](docs/EMBEDDABLE_FETCH.md).

### Operate it in production (v0.5.0)

Interrupted downloads resume by default; the cache heals itself; one command
diagnoses a deployment:

```sh
# Resume whatever fetches were interrupted (bootstrap downloads continue
# via HTTP Range; chunk fetches continue from the cache have-set)
./target/release/cavs-client resume --cache ./cache

# Cache maintenance: re-hash everything (corrupt entries -> quarantine),
# re-fetch an asset's missing/corrupt chunks, evict LRU to a size budget
./target/release/cavs-client cache verify --cache ./cache
./target/release/cavs-client cache repair http://127.0.0.1:8990 build_v2 --cache ./cache
./target/release/cavs-client cache gc --cache ./cache --max-size 10GiB

# Diagnose: container integrity, manifest, bootstrap sidecar, store, cache
./target/release/cavs doctor build_v2.cavs --store ./store --cache ./cache

# Prove every decoder rejects corruption cleanly (20-row mutation matrix)
./target/release/cavs test corrupt build_v2.cavs --out corrupt-report.json

# Reproducible large-build benchmarks (deterministic synthetic datasets)
./target/release/cavs bench gen --out ./ds --size 1GiB
./target/release/cavs bench suite --dataset ./ds --out ./results
```

Failures carry stable error codes (`CAVS-E-BOOTSTRAP-HASH-MISMATCH`,
`CAVS-E-CACHE-CORRUPT-RECOVERABLE`, `CAVS-E-NETWORK`, …) so launchers and
scripts can decide retry/repair/give-up without parsing prose.

### Hybrid reconstruction (v0.6.0)

The previous installed version is now a first-class byte source: a client
with an **empty cache but the old build on disk** copies verified ranges
from it and downloads only what changed (measured: −90.3 % wire on a small
update vs v0.5's cold path, −99.98 % on a shifted build — see
[docs/HYBRID_RECONSTRUCTION.md](docs/HYBRID_RECONSTRUCTION.md)):

```sh
# Update reusing the old install directly (works with a cold cache)
./target/release/cavs-client fetch http://127.0.0.1:8990 build_v2 \
  -o ./install --cache ./cache --previous-artifact ./install/build_v1.bin

# Compact old-version signatures (~0.07% of the source)
./target/release/cavs signature export build_v1.cavs -o build_v1.cavssig
./target/release/cavs pack --raw build_v2.bin --against-signature build_v1.cavssig -o v2.cavs

# Directory/container mode (preview): per-file dedup, staged installs,
# unchanged (modded) files untouched
./target/release/cavs pack-dir ./Build_v2 -o build_v2.cavs
./target/release/cavs-client fetch http://127.0.0.1:8990 build_v2 -o ./InstalledBuild --cache ./cache

# Compare against a block-based delta patcher (and xdelta3/bsdiff if present)
./target/release/cavs bench delta --old build_v1.bin --new build_v2.bin --out results/delta
```

Already-current outputs are detected and skipped (no-op: 0 bytes), every
copied range is BLAKE3-verified before it is written, and a corrupt old
install demotes to cache/network instead of failing.

### Offline toolkit (v0.7.0)

Sign, preview, diff, apply and verify updates locally — no CAVS server. The
offline apply uses the same verified reconstruction model as the online
client, so a `.cavsplan` update is byte-identical or it fails:

```sh
# 1. Describe the released version once (compact, ~0.07% of the source)
./target/release/cavs signature export ./Build_v1 --raw -o build_v1.cavssig

# 2. See what the next build changes before publishing anything
./target/release/cavs preview ./Build_v2 --against build_v1.cavssig --changes-only

# 3. Produce a deterministic offline update plan (a portable patch)
./target/release/cavs diff-plan ./Build_v1 ./Build_v2 -o update.cavsplan --report plan.md

# 4. Apply it in place — staged, journaled, verified, mod-friendly
./target/release/cavs apply --old ./InstalledBuild --plan update.cavsplan --inplace --verify

# 5. Check any install against a known-good signature (mods tolerated)
./target/release/cavs verify-install ./InstalledBuild --signature build_v2.cavssig --allow-extra-files

# Identify/inspect any CAVS file
./target/release/cavs file update.cavsplan
./target/release/cavs ls build_v1.cavssig
```

Directory builds are first-class: `cavs pack-dir ./Build -o b.cavs --ignore
'*.pdb' --ignore 'logs/'` (also reads a root `.cavsignore`). Measured on a
128 MiB build, the offline `.cavsplan` update is **2.51 MiB** (directory) and
**1.94 MiB** (single artifact) — matching butler's offline patch while
applying with a streaming ~8 MiB memory budget. Benchmark it yourself:

```sh
# Every delivery route for one transition (butler + pairwise proxies optional)
./target/release/cavs bench routes --old ./Build_v1 --new ./Build_v2 \
  --butler-bin ./butler --include-pairwise-proxy --out results/routes

# Fair external butler offline diff/apply/verify harness
./target/release/cavs bench butler-offline --old ./Build_v1 --new ./Build_v2 \
  --butler-bin ./butler --out results/butler

# Many-version storage: store-once vs per-pair patches
./target/release/cavs bench version-stream --out results/stream --versions 10
```

The butler harness measures butler's **offline/default** patch, not the
backend-optimized patch; bsdiff/xdelta3 results are labeled as an optimized
pairwise **proxy**. Full tables and framing:
[docs/ROUTE_BENCHMARKS.md](docs/ROUTE_BENCHMARKS.md),
[docs/BUTLER_COMPARISON.md](docs/BUTLER_COMPARISON.md),
[docs/OFFLINE_TOOLKIT.md](docs/OFFLINE_TOOLKIT.md).

### Delivery planner & optimized sidecars (v0.8.0)

```sh
# Publish a release in one pass: container + signature + plan +
# optimized sidecar, preceded by a preview (renames, blob warnings)
./target/release/cavs publish-dir ./Build_v2 --previous ./Build_v1 --out-dir releases/

# Per-file optimized sidecar for a hot pair, with the reasoning written out
./target/release/cavs optimize-patch --old ./Build_v1 --new ./Build_v2 \
  --algo auto --compression auto --explain-strategies why.md -o v1_to_v2.cavspatch

# Which pairs deserve a sidecar (hot pairs only — never the all-pairs O(N²) graph)
./target/release/cavs patch-policy --versions v1,v2,...,v10 --distribution shares.json

# Pick the route for one client state under a device profile
./target/release/cavs route-plan --installed ./InstalledBuild --new ./Build_v2 \
  --patch v1_to_v2.cavspatch --profile low-memory

# The proof report: every CAVS route vs the complete butler pipeline
# (default diff AND rediff --rediff-quality 9), honest verdicts included
./target/release/cavs bench full-pipeline --old ./Build_v1 --new ./Build_v2 \
  --butler-bin ./butler --include-pairwise --out results/pipeline

# Prove interrupted applies never break an install
./target/release/cavs test apply-recovery --old ./Build_v1 --new ./Build_v2
```

Measured on the 126 MiB directory release: CAVS auto-route ties the
optimized external patch on bytes (2.51 MiB) and apply time while using
**4.2× less memory** and generating **21× faster**; on a shifted 128 MiB
artifact it wins every column (4.21 KiB vs 11.39 KiB, 2.2× faster apply,
12% of the RAM); the compressed-blob weak case now routes through a
byte-level delta automatically (2.53 MiB where block routes paid
21.9 MiB). Planner and sidecar details:
[docs/DELIVERY_PLANNER.md](docs/DELIVERY_PLANNER.md),
[docs/PAIRWISE_SIDECARS.md](docs/PAIRWISE_SIDECARS.md).

### SteamPipe-style local analysis (v0.9.0)

```sh
# The numbers: how would a fixed-1MiB chunk model price this update?
./target/release/cavs bench steampipe-style ./Build_v1 ./Build_v2 --out results/

# The diagnosis: why is it expensive, and what should change?
./target/release/cavs analyze steampipe ./Build_v1 ./Build_v2 --out analysis.md
./target/release/cavs analyze-packs ./Build_v1 ./Build_v2 --out packs.md
./target/release/cavs analyze godot-pck old.pck new.pck --out pck.md
./target/release/cavs optimize-layout ./Build_v1 ./Build_v2 --write-plan layout.json

# The decision: every route measured, one recommended, before you ship
./target/release/cavs publish-preview ./Build_v2 --previous ./Build_v1 --routes all
./target/release/cavs io-estimate ./Build_v1 ./Build_v2
./target/release/cavs plan-update --from ./v1 --to ./v2 \
  --client-state has-previous-install,slow-hdd --policy hdd_friendly

# The workspace: SteamPipe-like depots/branches/builds as local metadata
./target/release/cavs workspace init ./ws --app my-app
./target/release/cavs depot add windows --workspace ./ws --platform windows
./target/release/cavs branch add beta --workspace ./ws
./target/release/cavs build create --workspace ./ws --branch beta \
  --depot windows=./Build/Windows --label v1
./target/release/cavs depot analyze-sharing --workspace ./ws
./target/release/cavs install-plan --workspace ./ws --branch beta \
  --platform windows --owned base,dlc1 --from build_1001
./target/release/cavs serve ./ws --port 8990   # dev-only content server

# Release authenticity (not DRM)
./target/release/cavs build sign build.cavs --key cavs.key
./target/release/cavs build verify build.cavs --pub cavs.pub
```

Measured on the pathology suite: the same 64 KiB edit costs **1 MiB or
the whole 32.88 MiB pack** under the fixed model depending only on
layout — and the analyzer names the cause and the fix; the CAVS
`.cavsplan` for the shifted pack is **7.4 KiB**. A ~3-byte change in a
256 MiB pack still costs **512 MiB of local I/O** unless the pack is
split. The estimates use a public fixed-1MiB model — SteamPipe-*style*,
never Valve's exact implementation
([docs/STEAMPIPE_STYLE_MODEL.md](docs/STEAMPIPE_STYLE_MODEL.md)) — and
there is deliberately no separate `steam-analyzer` product
([docs/WHY_NO_STEAM_ANALYZER_PRODUCT.md](docs/WHY_NO_STEAM_ANALYZER_PRODUCT.md)).
Full story: [docs/STEAMPIPE_COMPARISON.md](docs/STEAMPIPE_COMPARISON.md),
[docs/BUILD_UPDATE_ANALYZER.md](docs/BUILD_UPDATE_ANALYZER.md).

See [`game-engine-plugins/godot-plugin/README.md`](game-engine-plugins/godot-plugin/README.md) for an example of embedding the client in a host runtime.

### Patch policy benchmark (v1.1.0)

```sh
# A deterministic 10-version release stream to measure against
./target/release/cavs bench gen-stream --out builds --versions 10 --size 32MiB

# Compare practical patch policies (adjacent, ladder, base hub, hot pairs,
# all-pairs baseline) against CAVS routes under a traffic model
./target/release/cavs bench patch-policy --versions-dir builds --version-glob 'v*' \
  --policies adjacent,ladder,base,hot-pairs,all-pairs,cavs \
  --traffic-model adjacent-heavy --out results/patch-policy

# Hot pairs under a storage budget, and skip-heavy user behavior
./target/release/cavs bench patch-policy --versions-dir builds \
  --hot-pairs latest:5 --patch-storage-budget 2x-latest-build \
  --traffic-model skip-heavy --out results/budget

# Replay a different traffic model on the measured graph, or inspect one path
./target/release/cavs patch-policy simulate --graph results/patch-policy/patch_graph.json \
  --traffic-model major-release
./target/release/cavs patch-policy explain --graph results/patch-policy/patch_graph.json \
  --from v01 --to v09 --policy ladder
```

All-pairs pairwise patches require O(N²) patches only if every old→new
jump must be served in one direct step. Real systems use adjacent diffs,
sparse ladders, base-version policies, or hot-pair policies instead —
CAVS benchmarks those practical policies and compares them against its
content-addressed, cache-aware update routes. Full docs:
[docs/PATCH_POLICY_BENCHMARK.md](docs/PATCH_POLICY_BENCHMARK.md),
[docs/PATCH_GRAPH_POLICIES.md](docs/PATCH_GRAPH_POLICIES.md),
[docs/TRAFFIC_MODELS.md](docs/TRAFFIC_MODELS.md).

### SDKs — Go, Kotlin, Node (v1.2.0)

Integrate CAVS into backend services and CI/CD pipelines programmatically —
the SDKs load the same compiled Rust core the CLI uses through a stable C ABI
(`cavs-ffi`), so there is no shelling out and no CAVS-hosted infrastructure.
All three expose the same operations — `analyze`, `preview`,
`packDirectory`, `createPlan`, `applyPlan`, `verifyInstall`, `benchmark`,
`estimateSavings` and (v1.4.0) `fetchStatic` — with a consistent error model.

```go
// Go — github.com/orelvis15/cavs-oss/sdks/go
client, _ := cavs.New()
defer client.Close()
preview, _ := client.Preview(ctx, cavs.PreviewRequest{OldPath: "Build_v1", NewPath: "Build_v2"})
fmt.Println(preview.RecommendedRoute)
```

```kotlin
// Kotlin/JVM — io.github.orelvis15:cavs-sdk:1.2.0  (Java 22+)
CavsClient.create().use { cavs ->
    val preview = cavs.preview(PreviewRequest(oldPath = "Build_v1", newPath = "Build_v2"))
    println(preview.recommendedRoute)
}
```

```ts
// Node/TypeScript — @orelvis15/cavs-sdk
const cavs = new CavsClient();
const preview = await cavs.preview({ oldPath: "Build_v1", newPath: "Build_v2" });
console.log(preview.recommendedRoute);
cavs.close();
```

Full references: [docs/SDKS.md](docs/SDKS.md) (overview + architecture),
[docs/SDK_GO.md](docs/SDK_GO.md), [docs/SDK_KOTLIN.md](docs/SDK_KOTLIN.md),
[docs/SDK_NODE.md](docs/SDK_NODE.md),
[docs/SDK_NATIVE_ABI.md](docs/SDK_NATIVE_ABI.md).

## Components

- **`cavs`** (CLI): package files/builds into `.cavs` (FastCDC + zstd +
  optional Ed25519 signature) with payload classification, `--profile auto`
  chunk-profile selection, `--bootstrap` cold-install artifacts and a
  `sweep` cost report; inspect, verify, reconstruct, manage a global
  store (`add` / `rm` / `gc` / `stat` / `verify` / `export`, loose or
  packfile layout), inspect manifest formats
  (`manifest export` / `manifest bench`), diagnose deployments
  (`doctor`), run the corruption matrix (`test corrupt`) and generate/run
  reproducible large-build benchmarks (`bench gen` / `bench suite`). v0.7.0
  adds the offline toolkit (`signature` / `preview` / `diff-plan` / `apply` /
  `verify-install` / `file` / `ls`, `pack-dir` with `.cavsignore`,
  `optimize-patch`) and the external benchmark harnesses
  (`bench butler-offline` / `pairwise-proxy` / `routes` / `version-stream`).
  v0.8.0 adds the delivery planner (`route-plan`), per-file optimized
  sidecars (`optimize-patch --algo auto`, `apply-patch --memory-budget`),
  the hot-pair policy (`patch-policy`), one-command publishing
  (`publish-dir`), the full external pipeline harnesses
  (`bench butler-full` / `full-pipeline`) and the interrupted-apply matrix
  (`test apply-recovery`).
- **`cavs-server`**: stateful HTTP/HTTPS origin. Per-session have-set,
  inline/reference planning, dual-route decision (bootstrap vs chunks) per
  client, manifest format negotiation (compact binary v2 / JSON v1), CVSP
  binary batches, coalesced packfile range reads with read-efficiency
  metrics, immutable CDN-cacheable chunk and bootstrap endpoints (ETag,
  immutable Cache-Control, HTTP Range for resume), and a `--store` mode.
- **`cavs-client`**: native streaming client with a persistent cache and
  atomic, verified reconstruction; negotiates the compact binary manifest,
  takes the bootstrap route when offered (seeding its cache); resumes
  interrupted downloads from a crash-safe journal, retries transient
  failures with backoff, and maintains its own cache
  (`cache verify` / `repair` / `gc`).
- **Godot plugin**: `CavsClient` in pure GDScript (no native binaries) —
  install as an addon, mount packs at runtime. See
  [`game-engine-plugins/godot-plugin/README.md`](game-engine-plugins/godot-plugin/README.md).
- **SteamPipe-style analysis** (v0.9.0, inside the `cavs` CLI): update-cost
  estimation under a public fixed-1MiB model (`bench steampipe-style`),
  layout diagnosis with recommendations (`analyze steampipe`,
  `analyze-packs`, `analyze godot-pck`, `optimize-layout`), release
  previews across every route (`publish-preview`), local disk I/O
  estimates (`io-estimate`), a policy-scored route planner
  (`plan-update`), a local app/depot/branch/build workspace with sharing
  analysis and install-plan simulation, a dev content server
  (`cavs serve`) and local signing/encryption (`build sign` /
  `build encrypt` — authenticity, not DRM). See
  [docs/BUILD_UPDATE_ANALYZER.md](docs/BUILD_UPDATE_ANALYZER.md).

## Contributing

Contributions are welcome — see [`CONTRIBUTING.md`](CONTRIBUTING.md) for setup,
the PR workflow, and the checklist. Every PR runs CI (format, clippy, tests).
Releases are cut by the maintainer by pushing a version tag (`v*`), which
triggers the [release workflow](.github/workflows/release.yml) to build and
publish versioned binaries for Linux, macOS and Windows.

## License

Licensed under the **Apache License, Version 2.0** — see [`LICENSE`](LICENSE)
and [`NOTICE`](NOTICE). You may use, modify and distribute this software freely
under its terms; it includes an express patent grant.
