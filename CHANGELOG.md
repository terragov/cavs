# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`cavs-store`: the caller chooses what a sync waits for.** `SyncMode`
  (`Full` — `F_FULLFSYNC` on macOS, what Rust's `sync_all` does there;
  `Fsync` — `fsync(2)`, what SQLite and PostgreSQL do by default; `Barrier`
  — `F_BARRIERFSYNC`, ordering without a flush) and
  `GlobalStore::set_sync_mode`, applied to the packfile, the record pack, the
  ledger journal and snapshot and their directory syncs.
  `PackWriter::finish_with` takes the mode; `finish` stays `Full`. The write
  order — chunks before the ledger that names them — is kept in every mode.
  The default is unchanged: `Full`.

### Changed

- **`cavs-store`: a save costs what it changed, not what the store holds.**
  - The monolithic ledger no longer rewrites `index.bin` on every save. A
    save appends one BLAKE3-sealed record of the entries it touched to
    `index.log`, and the snapshot is rewritten only once the journal has
    outgrown it (never below 1 MiB, see `JOURNAL_MIN_BYTES`;
    `GlobalStore::set_journal_budget` overrides it, `0` restores a snapshot
    per save). Open replays the journal over the snapshot; a torn or corrupt
    tail is cut back to the last whole record, and a recovery from
    `index.bin.prev` replays the journal that snapshot had rotated away
    (`index.log.prev`). `IndexReport` gained `journal_bytes` and
    `snapshot_bytes`; `store index-inspect` prints them.
  - The records of a publish batch go into one content-addressed, immutable
    file, `assets/records/<hex>.cavsrec`, and the ledger holds each asset's
    byte range in it — one file and one fsync per publish instead of four
    filesystem calls per asset in a directory holding every asset the store
    has. `get_asset` and the exports read the range; flat `assets/<name>.json`
    files from earlier publishes are still read, and are replaced in place by
    a republish. `gc` deletes record packs no live asset points into. The
    segmented index keeps flat records, and `index-migrate` writes them out
    for every asset in a pack first.
  - The snapshot format is v2 (record locations per asset); a 1.7 reader
    rejects it rather than opening a snapshot and missing the saves its
    journal holds. A v1 snapshot opens and its first save writes v2.
  - The snapshot read-back after a rewrite checks the seal instead of
    decoding the whole ledger.
  - Measured with `cavsdb-bench commit-cost` (200 objects of 4 KiB per
    transaction, 60,000 objects): commit p50 went from 63–66 ms at an empty
    repository and 121–127 ms at 50,000 objects to 37 ms and 42 ms; the
    ledger's share of a commit no longer grows with the repository.

## [1.7.0] — Round 3: metadata batching, segmented index, adaptive fetch

### Added

- **Round 3A — session meta-packs & batching metadata resolver.**
  - The publisher writes one `meta/packs/<id>.cmeta` per push (zstd JSON:
    manifest + chunk locations of every uploaded object) plus a
    self-healing `meta/index.json` (oid → pack; rebuilt from the packs
    when unreadable). Content-addressed and immutable.
  - `cavs_fetch::MetadataResolver`: L1 in-process + L2 disk metadata
    caches (L2 validated against the remote's oid → pack mapping, so
    re-pushes/repacks never serve stale locations), meta-pack prefetch of
    push siblings, singleflight on the miss path, 5 s negative cache for
    remotes without a meta index, per-asset fallback (full compatibility
    with pre-Round-3 trees).
  - Phase timing in `FetchStats` (`metadata_ms/plan_ms/payload_ms/
    reconstruct_ms`, `metadata_requests`); the LFS agent logs a
    per-object and per-session breakdown.
  - cavs-server: `POST /v1/metadata/batch` (≤128 objects, per-object
    partial responses) + `cavs_metadata_batch_*` metrics.
- **Round 3B — segmented, mmapped index (opt-in).**
  - `cavs store <dir> index-migrate`: migrates `index.bin` to
    `index/` — immutable BLAKE3-sealed segments (fixed-stride sorted
    records, mmap + binary search; the chunk table never loads into RAM),
    per-generation `root.idx` + atomic `CURRENT` swap, begin/commit WAL,
    delta segment per publish session, threshold-driven compaction,
    current+previous generation retention, per-segment corruption
    detection. `index.bin.pre-migration` is kept for rollback.
  - `cavs store <dir> index-inspect` reports mode/generation/segments.
- **Round 3B — chunk-map v2 by runs.** Meta-packs encode object locations
  as runs of physically contiguous chunks (pack + start offset once,
  implicit offsets, uniform flags collapse to a scalar) — >30% fewer
  metadata bytes than per-chunk entries (tested). Dual reader; per-asset
  `chunk-map.json` stays v1 for older clients.
- **Round 3C — AIMD adaptive download concurrency.** `connections = 0`
  (the new agent default) grows/shrinks the worker pool between 2 and 64
  (+1 per clean 1 s window, halve on pressure: failed ranges, short
  reads, HTTP 429/503; 1 s cooldown). `CAVS_FETCH_CONCURRENCY=auto|N`
  override; fixed pools unchanged. New `FetchStats` fields
  (`concurrency_mode/peak`, `aimd_decreases`).
- **Round 3D — fragmentation telemetry & copy-on-write repack.**
  `cavs store <dir> fragmentation` (per-pack live/dead bytes, small-pack
  ratio, comparative score) and `cavs store <dir> repack [--dry-run]`:
  merges packs <8 MiB, compacts packs >30% dead, rewrites live chunks
  into fresh packs, swaps the ledger generation and quarantines old packs
  (recoverable; reads keep working; crash-safe at every step).
- Format documentation: `docs/ROUND3_FORMATS.md`.

### Changed

- `GlobalStore::chunk_info` now returns an owned `ChunkInfo` (required by
  the mmapped chunk table); `cavs-lfs-agent --connections` defaults to
  `0` (adaptive).

## [1.6.0] — Session publish, BG4 codec, coalesced range fetch, hardened ledger

### Added

- **Round 2.1 hardening (pre-merge P0s).**
  - *Crash-safe ledger writes*: `index.bin` is staged to a temp file,
    fsynced, read back and seal-verified before an atomic rename; the
    outgoing snapshot is kept one generation as `index.bin.prev` and the
    open path falls back to it when the current snapshot is corrupt or
    missing. Both-generations-corrupt is a clear `IndexCorrupt` error,
    never a silent empty store.
  - *Formal `index.bin` header*: self-describing header (version,
    header_size, record_size, generation, created_at) — readers reject
    snapshots written by a newer CAVS with a clear message, header growth
    within v1 stays compatible, and untrusted counts can no longer drive
    allocations past the file size.
  - *GC quarantine*: packs are never deleted directly. Dead and orphaned
    packs move to `quarantine/` (stamped), get deleted only after they
    also age out of quarantine, and are restored automatically — by the
    sweep or at open — if the ledger references them again (e.g. after an
    `index.bin.prev` recovery).
  - *Read-amplification cap in coalescing*: a range group's gap bytes are
    capped at 15% of its useful bytes, so a sparse update can no longer
    download multiples of what it needs; per-group amplification is
    bounded at 1.15× by construction (`FetchStats.useful_bytes` exposes
    it).
  - *Global download backpressure* (cavs-fetch): a process-wide weighted
    semaphore caps wire bytes in flight (default 128 MiB,
    `CAVS_FETCH_MAX_INFLIGHT_BYTES` to override), so N concurrent fetches
    can't stack N × connections × 8 MiB of buffers.
    `FetchStats.throttle_waits` counts backpressure events.
  - *Selective retries*: transport errors and short reads get one
    transparent retry; a chunk failing BLAKE3 inside a coalesced range is
    re-requested alone (fresh request for its exact subrange) instead of
    repeating the whole range, then fails with a stable
    `CAVS-E-*` diagnosis. `FetchStats.requests`/`selective_retries` count
    them.
  - *WAN benchmark harness*: `range_server.py` honours `LATENCY_MS` to
    emulate per-request WAN latency in `http-bench.sh`.
- **Coalesced range fetch** (cavs-fetch + serverless client): missing
  chunks of the same pack are sorted and fetched in Range GETs of up to
  8 MiB (gaps ≤ 64 KiB ride along); each chunk is still BLAKE3-verified
  individually. Cold clone over HTTP: 6,179 → 46 requests on a 104 MiB
  asset (−99.3%), −85% on 250 small files (`bench/results/perf-round2-v1`).
- **Binary store ledger** (`index.bin`, CAVSIDX1): compact fixed-record
  snapshot with a BLAKE3 seal replaces pretty-JSON `index.json` — −77%
  size and −48% load time at 1M chunks; legacy stores are read and
  migrated on the next save.
- **Orphan-pack GC**: `gc()` also sweeps sealed packs no ledger entry
  references (residue of a push killed between pack rollover and commit),
  honoring the grace period against pack mtime.
- `bench/http-bench.sh` + `bench/range_server.py`: HTTP cold-clone
  benchmark (requests + wall time) for A/B-ing agent binaries.

- **BG4 chunk codec** (`CHUNK_FLAG_BG4`, Xet-inspired): per-chunk choice of
  plain zstd vs a byte-grouping-of-4 pretransform + zstd, attempted only
  when plain zstd underperforms — a large win on float/int payloads (model
  weights, vertex buffers, audio samples). Opt-in at the `Writer`
  (`set_bg4`); the LFS agent enables it by default (`--no-bg4` to disable).
  Decoded by cavs-format, cavs-fetch, the serverless client and store
  verify; cavs-server decodes BG4 server-side, so the wire protocol and the
  non-Rust SDKs are unchanged.
- **Session-batched publish** (`GlobalStore::begin_publish_batch` /
  `commit_publish_batch`, Xet-style finalize): inside a batch, publishes
  update only the in-memory ledger, the ingest pack aggregates across
  assets (rollover at the preferred pack size instead of one pack per
  asset), and `index.json` is written once at commit. A crash before the
  commit leaves the on-disk store untouched.

### Changed

- **cavs-lfs-agent publishes at session finalize.** Uploads only ingest;
  the export tree refresh and the ledger commit happen once per push, at
  terminate. A many-object push no longer rewrites the store ledger per
  object nor produces one pack per object; an interrupted push publishes
  nothing and the retry repairs (new crash tests cover this).
- New `tensor` benchmark scenario (float32 random-walk weights) in
  `bench/gen.py`.

## [1.5.1] — LFS agent progress fixes

### Fixed

- **cavs-lfs-agent: monotonic progress events.** The download progress
  throttle raced across fetch worker threads and could emit a lower
  `bytesSoFar` after a higher one; the check-and-send is now atomic
  (mutex), so events are strictly monotonic. Caught by CI on Linux — the
  integration tests assert monotonicity and now pass deterministically.
- **cavs-lfs-agent: upload progress accounting.** The three upload
  milestones over-reported `bytesSinceLast` (deltas summed to ~110 % of the
  object); they now sum exactly to the object size.
- e2e script prints the captured git/git-lfs/agent stderr when a step
  fails, instead of a bare exit code.

## [1.5.0] — Git LFS transfer agent

Everything below is measured in the frozen `benchmark-v1`
(`core/cavs-lfs-agent/bench/RESULTS.md`, raw data committed): vs vanilla
Git LFS, −76…−90 % storage (single copy) and −52…−97 % update download on
versioned binaries, warm clones at 0 new bytes, cross-repo dedup −89 %;
14/14 sha256 verification gates.

### Added

- **Git LFS transfer agent (new crate `cavs-lfs-agent`).** A Git LFS
  *standalone custom transfer agent* that replaces LFS's whole-file transfer
  and storage with CAVS chunk-level dedup: uploads pack each LFS object as a
  single raw track (asset name = track name = the LFS sha256 oid, so
  `cavs-fetch`'s existing `sha256:` meta verification checks the oid end to
  end), ingest into a shared `GlobalStore` at the remote and refresh a static
  export before reporting success; downloads run through the embeddable
  `cavs-fetch` engine (local chunk cache, concurrent range fetch, BLAKE3 +
  sha256 verification). Measured in the committed e2e: a ~3 MiB change to a
  22 MiB file stores/sends ~3.3 MiB instead of the whole file, and a warm
  pull moves ~0.5 MiB. Directory and `file://` remotes are read/write (with
  a cross-process file lock; bare git repos auto-derive `<repo>/cavs/`);
  `http(s)://` remotes are read-only for CDN-served clones. Protocol-level
  integration tests plus a real git+git-lfs e2e (`core/cavs-lfs-agent/e2e/`,
  also a CI job). See [docs/GIT_LFS.md](docs/GIT_LFS.md).
- **Size-tiered automatic chunking profile** in the agent (`--profile auto`,
  the default): <128 MiB → fastcdc-16k, <512 MiB → fastcdc-64k, else
  fastcdc-128k — tuned from the committed profile sweep; a pure function of
  size so chunk boundaries (and cross-version dedup) stay stable.
- **Benchmark harness + frozen `benchmark-v1`**
  (`core/cavs-lfs-agent/bench/`): plain git vs vanilla Git LFS vs the CAVS
  agent over deterministic datasets — storage, per-version growth, update /
  cold / warm-clone download, storage breakdown, cross-repo dedup, and
  crash-recovery tests (agent killed mid-upload/mid-download; store
  self-repairs).
- `GlobalStore::export_asset()` — incremental single-asset export
  (missing packs + that asset's record/chunk-map/manifest), O(asset) not
  O(store).

### Changed

- **Per-asset export + session-scoped store in the agent**: one lock and
  one store open per push session; each upload exports only its own asset.
  250-object push: 19.9 s → 6.5 s.

- **Shared ingest/manifest library APIs (refactor).** `cavs store add`'s
  container→store ingest moved into `cavs-format` as
  `ingest_into_store()`/`IngestStats`, and the static-export manifest
  builder moved into `cavs-store` as `GlobalStore::asset_manifest()` /
  `export_static_manifests()` (new `cavs-proto` dependency); cavs-cli now
  calls the shared APIs. `export_object_store()` skips copying packs/indexes
  already present with the same length at the destination — re-exports into
  the same tree are effectively incremental.

## [1.4.0] — Serverless delivery, parallel downloads & embeddable client

Everything measured below was verified on real version pairs and the
deterministic synthetic suite; every reconstruction is byte-identical.

### Added

- **Serverless / CDN-only delivery.** `cavs store export --static-plans` now
  writes, per asset, a `manifest.json` (reconstruction structure) alongside
  the existing `chunk-map.json` (now carrying absolute pack byte offsets), so
  the exported tree is fully self-describing. The new
  `cavs-client fetch-static <url|dir> <asset>` installs and updates a build
  **straight from that static tree — with no `cavs-server`** — planning the
  missing set locally and pulling only changed chunks over concurrent HTTP
  Range requests, verified end to end. Host the tree on S3 / R2 / GitHub
  Pages / nginx / a local folder. See
  [docs/SERVERLESS_DELIVERY.md](docs/SERVERLESS_DELIVERY.md).
- **`cavs-fetch` — an embeddable fetch engine (new crate).** The serverless
  install/update path as a library: content-addressed cache, concurrent range
  fetch, BLAKE3-verified reconstruction, a progress callback, cooperative
  cancellation and optional Ed25519 signature enforcement. Launchers and applications
  link it to self-update in-process. Exposed through a new SDK operation
  **`fetchStatic`** (Go, Kotlin, Node) and, since the C ABI is generic
  JSON-in/out, through `libcavs_sdk` with no ABI change. See
  [docs/EMBEDDABLE_FETCH.md](docs/EMBEDDABLE_FETCH.md).
- **Unity and Unreal plugins (reference integrations, _untested_).** A Unity
  UPM package (C# P/Invoke over `libcavs_sdk`) and an Unreal runtime module
  (`UCavsClient`, C++ over `cavs_sdk.h`), both driving `fetchStatic` for
  in-process self-update with progress and cancellation. They compile against the
  C ABI and mirror the shipping SDK bindings but are **not yet validated on a
  device** — clearly marked as such in their READMEs.
- **Content-addressed parallel chunk download** in `cavs-client`
  (`--connections N`, container payloads). Instead of the sequential
  session/batch round-trips, the client computes its own missing set and
  downloads immutable chunks **concurrently by hash** from the edge-cacheable
  chunk endpoint. Measured **−26% wall time at 4 connections** on a localhost
  origin (1.78 s → 1.32 s on a 60 MB build); the win grows on latency-bound
  links, where the sequential path pays a round trip per batch. Egress is
  byte-for-byte identical (compression preserved via a new stored-bytes
  negotiation on the chunk endpoint). **Opt-in**: the default keeps the
  session/batch path so a single packfile origin retains its read-coalescing;
  enable `--connections N` when a CDN fronts the origin.
- **Small chunk profiles `fastcdc-64k-n3` / `fastcdc-128k-n3`** — the 64 KiB /
  128 KiB averages with FastCDC normalization level 3. New labels, so existing
  `fastcdc-64k` / `fastcdc-128k` streams keep their exact published
  boundaries. On a shifted-and-edited 24 MiB pair, `fastcdc-128k-n3` cut the
  update payload **−62%** vs `fastcdc-128k` (625 KiB vs 1.64 MiB) for the same
  edits, at lower total storage; `fastcdc-64k-n3` is neutral there (its chunks
  are already small). `--profile auto` sweeps them and — because reuse is
  measured against a published stream's real chunk set — never switches a
  continuing stream's normalization mid-train.

### Changed

- `cavs-server`'s content-addressed chunk endpoint
  (`/api/assets/{asset}/chunks/{hash}`) serves the chunk **exactly as stored**
  (possibly zstd) with wire-metadata headers when the client sends
  `x-cavs-accept-stored: 1`; it still serves raw bytes to older clients. This
  gives the parallel path the same wire savings as the session path.
- Desktop app: the pack profile dropdown offers the new `-n3` profiles, and a
  new **Serverless CDN** section builds the `store export --static-plans` and
  `fetch-static` commands.

### Fixed

- `tool_metrics::available()` (used by `certify` and the benchmark harnesses)
  bounded the external-tool probe with a 1.5 s deadline and now kills the
  child, so a **GUI `godot` on `PATH` no longer hangs** the command
  indefinitely (a Homebrew `godot` opens its project manager on the probe flag
  and never exits).
- The release workflow's Maven Central deploy now tolerates an
  already-published version (like the npm and crates jobs), so a re-run — or a
  cancelled run whose async Portal publish already landed — is idempotent.

## [1.3.0] — Smaller updates & parallel packing

### Core algorithm improvements — measured, not projected

Every number below was measured on real version pairs (MechanicalFlower/
Marble 1.6.0→1.6.1 PCK, Godot editor 4.2→4.2.1 linux binary) and on the
deterministic synthetic suite; all reconstructions verified byte-identical.

### Added

- **Small chunk profiles `fastcdc-16k` / `fastcdc-32k`** (CLI `--profile`,
  `cavs sweep`, `--profile auto`, SDK `packDirectory`, desktop app). On the
  real Marble PCK pair they cut update egress **−65 % / −47 %** vs
  `fastcdc-64k` for ~+4 % / +2.6 % cold chunk-path egress and a few KiB of
  (binary v2) manifest. The new profiles use FastCDC normalization level 3
  (tight size distribution, worth ~−20 % more update egress at the same
  average); existing profiles keep their exact published boundaries.
- `cavs-chunker`: `ChunkMode::Cdc` gains a `norm` field (FastCDC
  normalization level). `NORM_DEFAULT` (= 1) reproduces the pre-existing
  boundaries bit-for-bit — pinned by a compatibility test.
- `cavs-format`: `Writer::add_chunks_parallel` — per-chunk BLAKE3 + zstd
  now run on all cores at pack time; ingest order and the output file are
  **byte-identical** to the serial path (pinned by test). Measured pack
  speedup: **3.2×** at zstd-3, **7.2×** at zstd-19 (Godot 100 MB binary).
  `pack`, `pack-dir` and the SDK `packDirectory` all use it.

### Changed

- `--profile auto` cost model: the manifest term now prices the binary v2
  manifest (~36 B/chunk, measured) instead of the JSON-era 150 B/chunk,
  which silently biased the sweep ~4× against the small-chunk profiles
  that win update egress.
- `--profile auto` with `--bootstrap` on a first version of update-heavy
  payloads (engine packs/archives) now starts the stream at `fastcdc-16k`
  instead of `fastcdc-64k`: the bootstrap already serves the cold path,
  and updates are 60–68 % cheaper from the first patch on. Existing
  streams keep continuity through `--prev`.
- With parallel ingest, per-chunk **zstd-19 becomes a practical choice**
  for release publishing (`--zstd-level 19`): **−9/10 % wire and storage**
  on both real pairs, identical client decode cost, server passthrough
  unchanged. The default stays zstd-3.
- Desktop app: the pack form now offers the 16k/32k profiles and explicit
  zstd levels (3 / 9 / 19 / none) — and sends valid `zstd-<level>` values
  (the previous "zstd" string was rejected by the SDK).

### Honest negatives (measured, not adopted)

- Per-release zstd dictionaries: −0.5–3.5 % — not worth the format change.
- Bootstrap long-distance matching / level 22: −0.03–1.15 % vs the
  current zstd-19 — already near-optimal.

## [1.2.0] — Language SDKs

CAVS now ships **SDKs for Go, Kotlin/JVM and Node/TypeScript** so publishers,
backend teams and CI/CD systems can drive the engine programmatically instead
of shelling out to the CLI. Each SDK loads the same compiled Rust core through
a new stable C ABI and exposes idiomatic, typed APIs for the eight
highest-value operations, with progress events and cancellation.

### Added

- `cavs-sdk-core`: a high-level JSON operation engine shared by the SDKs —
  `analyze`, `packDirectory`, `previewUpdate`/`compareRoutes`, `createPlan`,
  `applyPlan`, `verifyInstall`, `benchmark` and `estimateSavings`, behind a
  versioned request/response envelope with a stable `CAVS-E-*` error model.
- `cavs-ffi`: a minimal, stable C ABI (`cdylib`/`staticlib`) over
  `cavs-sdk-core` — context/result/job handles, a progress callback,
  cooperative cancellation, and the checked-in `cavs_sdk.h` header.
- **Go SDK** (`github.com/orelvis15/cavs-oss/sdks/go`): cgo binding with
  `context.Context` cancellation, `WithProgress` events and typed
  `*cavs.Error`.
- **Kotlin/JVM SDK** (`io.github.orelvis15:cavs-sdk`, Java 22+): a Foreign
  Function & Memory (JEP 454) binding, an `AutoCloseable` client,
  `CompletableFuture` async, kotlinx.serialization DTOs, published to Maven
  Central.
- **Node/TypeScript SDK** (`@orelvis15/cavs-sdk`): a Node-API binding,
  Promise-first API, `AbortSignal` cancellation, progress events and full
  TypeScript types, published to npm with per-platform native packages.
- Release automation builds `libcavs_sdk` for five targets (linux/macOS/
  windows × x86_64/arm64) with SHA-256 sidecars and publishes the SDKs to
  crates.io, npm, Maven Central and a Go submodule tag on each version.
- Documentation: `docs/SDKS.md`, `docs/SDK_GO.md`, `docs/SDK_KOTLIN.md`,
  `docs/SDK_NODE.md`, `docs/SDK_NATIVE_ABI.md`, and per-SDK pages on the site.

## [1.1.0] — The practical patch policy benchmark

CAVS now benchmarks practical pairwise patch policies instead of
comparing only against the all-pairs O(N²) worst case. The new
`cavs bench patch-policy` command compares adjacent diffs, sparse
power-of-two ladders, base-version policies, hot-pair storage-budget
policies, the all-pairs theoretical baseline, and CAVS
content-addressed/hybrid routes under realistic user update traffic
models — every pairwise number a real diff, applied and byte-verified.

### Added

- `cavs bench patch-policy`: policy-level comparison with real
  measurements per patch engine (`cavsplan` built in; `bsdiff`,
  `xdelta3`, `butler-offline` when installed — missing tools skip with
  a recorded reason, never fail the run).
- Adjacent pairwise policy benchmark.
- Sparse/dyadic ladder policy benchmark (`--ladder-mode aligned|dense`).
- Base-version hub policy benchmark (`--base-policy
  first|middle|latest-major|fixed:<id>|auto` — auto tests candidates
  and keeps the best under the traffic model).
- Hot-pair policy with storage budgets (`--hot-pairs latest:K |
  traffic-top:K`, `--patch-storage-budget 1GiB|2x-latest-build`,
  greedy savings-per-stored-byte selection, auditable in
  `storage_report.md`).
- All-pairs one-hop baseline, always labeled as theoretical.
- Traffic model simulation (`adjacent-heavy`, `skip-heavy`,
  `live-service-weekly`, `major-release`, `random`, `custom:file.toml`)
  with deterministic weighted expansion.
- Client states for the CAVS route: cold cache + previous install,
  warm cache.
- Patch graph export (`patch_graph.json`) plus `cavs patch-policy
  graph` (structure-only graphs), `simulate` (replay another traffic
  model with no re-diffing) and `explain` (show one path with sizes).
- Per-query path reports (`query_results.csv`), storage vs bandwidth
  reports, apply-chain/step-risk report, `tool_versions.json`.
- `cavs bench gen-stream`: deterministic v01…vNN release streams with
  configurable drift and an optional major content change
  (`--major-at`).
- Route planner: patch-chain risk penalty
  (`(patch_steps − 1) × STEP_RISK_WEIGHT`) so multi-step chains are
  never chosen over a one-step route for a few KiB
  (`cavs plan-update` reports `patch_steps` per route).
- New docs: `PATCH_POLICY_BENCHMARK.md`, `PRACTICAL_PAIRWISE_DIFFS.md`,
  `PATCH_GRAPH_POLICIES.md`, `TRAFFIC_MODELS.md`,
  `STORAGE_BUDGET_POLICIES.md`.

### Changed

- Updated README, BENCHMARKS and comparison docs to avoid implying that
  all pairwise diff systems require O(N²) patches: `O(N²)` is now
  described specifically as the all-pairs one-hop baseline, and
  practical pairwise policies (adjacent, ladder, base, hot pairs) are
  first-class benchmark baselines.
- `cavs patch-policy` gained subcommands (`graph`, `simulate`,
  `explain`); the existing sidecar planning flags are unchanged.

## [1.0.0] — The release certification suite

Highlights:

- New `cavs certify` release-readiness suite.
- Certification profiles: quick, standard, release, strict, ci.
- Deterministic reproducibility bundles.
- Route certification across cold install, warm cache, previous
  install, low RAM and slow HDD states.
- Godot PCK certification using the real plugin API
  (`CavsClient.fetch` / `fetch_async` / `ensure_pack`).
- Regression guard with absolute noise floor for timing/RAM metrics.
- v1.0.0 public trial guide at `/try`.

CAVS v1.0.0 adds `cavs certify`, a full release-readiness workflow for
updates. Certification verifies integrity, byte-identical
reconstruction, route selection, regression safety, Godot PCK
compatibility, workspace/depot install plans, SteamPipe-style analysis,
butler comparisons when available, disk I/O estimates and reproducible
reports — one command, stable exit codes, Markdown and JSON output.

### Results

Measured on the deterministic v1.0.0 suite (`docs/results/v1.0.0/`,
reproducible with `docs/results/v1.0.0/scripts/run-all.sh`):

- A 125.83 MiB directory build with ~2% drift certifies end-to-end in
  ~73 s under the strict profile: 2.26 MiB `.cavsplan` network
  (−98.2% vs full download), 214 ms verified byte-identical apply,
  0 files rewritten on no-op reapply, and every corruption smoke case
  (bit-flipped signature, plan and old input) rejected cleanly.
- The Godot case reconstructs the PCK byte-identically on every route
  (72.63 KiB chunk/hybrid wire for a one-resource edit vs a 2.50 MiB
  bootstrap) and the plugin API surface is verified unchanged.
- A 5-depot workspace (base/windows/linux/lang-es/dlc1) certifies
  promote/rollback previews, deterministic depot sharing and install
  plans per platform/language/ownership; only the changed base depot
  costs anything to update (9.25 MiB of 126.89 MiB).
- Re-certifying the same pair against its own baseline passes the
  regression guard with byte counts exact.

### Added

- `cavs certify` — the release-readiness orchestrator (artifact,
  directory, Godot PCK and workspace modes).
- `cavs certify integrity` — signatures, plans, path safety, mandatory
  byte-identical apply, no-op reapply, corruption smoke checks.
- `cavs certify routes` — planner decisions certified across the
  documented client-state matrix plus the measured route matrix with
  per-route output verification.
- `cavs certify regressions` — baseline comparison with configurable
  thresholds (5%/10%/20% defaults), absolute noise floors for timing
  and RAM metrics, and `--allow-regression metric=reason` exceptions;
  `--save-baseline` records baselines.
- `cavs certify godot` — byte-identical PCK on every route, PCK
  analyzer report, plugin API surface check, optional headless engine
  smoke test.
- `cavs certify workspace` — metadata, branches, promote/rollback
  previews, deterministic depot sharing, per-depot update cost and
  install-plan states.
- `cavs certify export-repro` — deterministic reproducibility bundle
  (tar.zst) with commands, environment, tool versions, reports and
  hashes; never private inputs unless `--include-inputs`.
- Certification profiles: quick, standard, release, strict and ci.
- CI-friendly exit codes (0 pass / 1 fail / 2 warnings / 3 missing
  dependency / 4 invalid input / 5 internal error), `--json-out`,
  `--fail-on-warning`.
- Certification report bundle: Markdown + JSON per section, plus
  `dependencies.json`, `environment.json`, `commands.sh` and hashed
  artifacts.
- Landing page "Try CAVS" guide (`/try`) with real commands and use
  cases; docs: CERTIFICATION, TRY_CAVS, CI, COMPATIBILITY,
  FILE_FORMATS, GODOT_PLUGIN, REPRODUCIBILITY, ROUTE_SELECTION,
  RELEASE_CHECKLIST.

### Stabilized

- Core CLI command families (pack, pack-dir, signature, preview,
  diff-plan, apply, verify-install, bench, analyze, publish-preview,
  plan-update, workspace/depot/branch/build, install-plan, certify).
- `.cavs`, `.cavsmf2`, `.cavssig`, `.cavsplan` and `.cavspatch` format
  documentation, with the v1.x compatibility policy
  (docs/COMPATIBILITY.md).
- Godot plugin runtime flow (`CavsClient.fetch` / `fetch_async` /
  `ensure_pack`).
- Route planner reports and certification JSON schemas
  (`cavs-certify-*/1`).
- Benchmark report structure (docs/results/v1.0.0).

### Notes

CAVS v1.0.0 remains a local build-update engine and analysis toolkit.
It is not a CDN, marketplace, DRM system, SteamPipe clone or itch.io
replacement.

## [0.9.0] — SteamPipe-style local analysis

CAVS now includes a SteamPipe-style update analyzer and build
optimization toolkit. This release does not create a separate
steam-analyzer product; instead, SteamPipe-style analysis is integrated
directly into the CAVS CLI (the previous standalone `steam-analyzer`
crate was removed — see `docs/WHY_NO_STEAM_ANALYZER_PRODUCT.md`).

### Results

Measured on the deterministic v0.9.0 suite
(`docs/results/v0.9.0/`, reproducible with
`cavs bench steampipe-cases` and the commands in its README):

- The same 64 KiB edit in a 32-asset pack costs 1.00 MiB (localized),
  1.88 MiB (TOC centralized) or the whole 32.88 MiB pack (shifted /
  shuffled / distributed-TOC) under the fixed-1MiB model; the CAVS
  `.cavsplan` for the shifted pack is 7.4 KiB.
- The analyzer diagnoses every pathology case (`asset_shuffling`,
  `toc_churn`, compressed blobs) and the recommended fixes recover
  94% / 75% fixed-chunk reuse when applied.
- A ~3-byte change in a 256 MiB pack downloads 2 MiB but costs 512 MiB
  of local disk I/O — slower than a full download on an HDD; splitting
  the pack cuts it to 128 MiB. `cavs io-estimate` flags exactly this.
- Windows ↔ Linux depots share 98.9% of their bytes; a demo owner with
  the full build installed downloads 0 B (cross-depot chunk reuse).
- The content-addressed store holds a 10-release stream in 22.43 MiB
  and serves any version jump; direct pairwise coverage would need 45
  patches.

### Added

- `cavs bench steampipe-style` for fixed-1MiB public-model update
  estimates (per-file or global scope, configurable chunk size and
  compression, artifact and directory modes, JSON/Markdown output).
- `cavs analyze steampipe` for diagnosing large updates and bad pack
  layouts: scattered churn, asset shuffling, distributed-TOC/offset
  churn, compression across asset boundaries, timestamp/build-id
  churn, oversized packs and new-content-in-old-pack — each finding
  with severity, estimated wasted bytes, cause, fix and expected
  improvement. `cavs analyze update-cost` as the numbers-only alias.
- `cavs publish-preview` for comparing update routes before release:
  measured full/CAVS routes, butler and bsdiff/xdelta3 when installed
  (missing tools skipped with a warning), the SteamPipe-style estimate
  row, release-readiness warnings and a recommended route.
- `cavs analyze-packs` for pack-file churn, shuffling, TOC and
  compression diagnostics (heatmaps at 64 KiB / 1 MiB / 8 MiB windows,
  scatteredness score, entropy, size advisories, engine hints).
- `cavs io-estimate` for local disk I/O and temporary-storage estimates
  per route, timed under configurable device profiles (HDD/SATA/NVMe).
- Local app/depot/branch/build workspace (`cavs workspace init`,
  `depot add`, `branch add`, `build create`) with atomic branch
  promotion/rollback and per-depot promotion previews.
- Shared depot/content reuse analysis (`cavs depot analyze-sharing`).
- Install-plan simulator by platform, language and ownership with
  cross-depot chunk reuse (`cavs install-plan`).
- Local content server for development/testing (`cavs serve`):
  branches, builds, depot files with HTTP Range, chunks by hash,
  update previews; explicitly not production hardened.
- Godot PCK analyzer (`cavs analyze godot-pck`): byte-level report
  plus, when the PCK directory is parseable (format v1/v2), the
  `res://` paths behind each changed region.
- Route planner with policy scoring (`cavs plan-update`):
  network_min / cpu_min / ram_min / disk_io_min / balanced /
  hdd_friendly / developer_fast, per client state; unavailable routes
  are never chosen.
- Patch-friendly layout optimizer (`cavs optimize-layout`), advisory
  only, with `--write-plan` JSON for automation.
- Local signing and optional encryption for release artifacts
  (`cavs build sign / verify / encrypt / decrypt / content-key`) —
  release authenticity, explicitly not DRM.
- `cavs bench steampipe-cases`: the deterministic pack-pathology
  benchmark (localized/shifted/shuffled/TOC/compression/new-pack/
  directory/Godot-PCK cases) measured under the model, real
  `.cavsplan`s and external tools.
- New library crates `cavs-analyzer` (model, heatmaps, entropy,
  detectors, recommendations) and `cavs-workspace` (workspace
  metadata, content indices, sharing math).
- New error codes: `CAVS-E-STEAMPIPE-MODEL-INVALID`,
  `CAVS-E-ANALYZE-PATH-MISSING`, `CAVS-E-ANALYZE-PATH-TRAVERSAL`,
  `CAVS-E-WORKSPACE-CORRUPT`, `CAVS-E-DEPOT-NOT-FOUND`,
  `CAVS-E-BRANCH-NOT-FOUND`, `CAVS-E-BUILD-NOT-FOUND`,
  `CAVS-E-INSTALL-PLAN-INVALID`, `CAVS-E-ROUTE-NOT-AVAILABLE`,
  `CAVS-E-GODOT-PCK-UNSUPPORTED`.
- Documentation: `STEAMPIPE_STYLE_MODEL.md`, `STEAMPIPE_COMPARISON.md`,
  `WHY_NO_STEAM_ANALYZER_PRODUCT.md`, `BUILD_UPDATE_ANALYZER.md`,
  `PACK_FILE_OPTIMIZATION.md`, `DEPOTS_BRANCHES_WORKSPACE.md`,
  `ROUTE_PLANNER.md`, `LOCAL_CONTENT_SERVER.md`,
  `GODOT_PCK_ANALYZER.md`, `IO_ESTIMATOR.md`.

### Changed

- Removed the standalone `steam-analyzer` crate and the `cavs-steam`
  binary; its analysis lives in `cavs analyze` / `cavs bench
  steampipe-style` with strictly more capability. Docs, README and the
  landing site no longer present a separate analyzer product.
- Workspace version 0.9.0.

### Notes

The SteamPipe-style model is based on public documentation and is not
Valve's exact SteamPipe implementation. It is intended to help
developers understand fixed-chunk update behavior, pack-file churn and
local update costs. Every report carries that labeling.

## [0.8.0] — Auto-route optimized delivery

CAVS v0.8.0 introduces auto-route optimized delivery: a planner that can
choose between chunks, hybrid reconstruction, offline plans, optimized
sidecars, bootstrap, full download, or no-op based on client state,
memory budget, and measured route cost.

In the v0.8.0 benchmark suite, CAVS auto-route matched or beat the
optimized baseline in network bytes, apply time, peak RAM, correctness,
multi-version storage, and arbitrary version jumps, while keeping
sidecar generation limited to selected hot pairs instead of requiring
all-pairs patches.

### Results

Measured on the reproducible v0.8.0 suite (synthetic builds, seed 5;
butler v15.28.0 default `diff` *and* optimized `rediff --rediff-quality
9` as baselines; bsdiff 4.3, xdelta3 3.2.0; Apple M3 Pro). Raw outputs,
environment, exact reproduction commands and known tradeoffs:
[docs/results/v0.8.0/](docs/results/v0.8.0/README.md).

- Matched optimized-baseline bytes on typical directory updates
  (2.51 MiB vs 2.51 MiB, apply within 5%) while generating 21× faster
  and using 4.2× less peak RAM (23 vs 97 MiB).
- Beat the optimized baseline on shifted artifacts: 4.21 KiB vs
  11.39 KiB, with faster apply and lower RAM.
- Closed the compressed-blob weak case with per-file xdelta3 routing:
  2.53 MiB where block routes paid 21.9 MiB on the same change.
- Reduced 10-version stream storage by 75% versus all-pairs patches
  (35.91 MiB store + hot pairs vs 144.23 MiB in 45 patches), still
  serving arbitrary version jumps.
- Enforced memory budgets at apply time: a 517 MiB-RSS bsdiff sidecar is
  refused under `--memory-budget 128MiB` and the planner serves the
  streaming plan route (27 MiB RSS) at comparable bytes.
- Preserved byte-identical output across every route, including 10
  SIGKILL-interrupted applies recovered via the journal.

Not claimed: the default `butler diff` remains faster to generate than a
full sidecar, its apply was marginally faster on the directory case, and
dedicated pairwise patches can still win bytes on inputs outside this
suite — see the tradeoffs table in the results directory.

### Added

- `.cavspatch` v2 (`CAVSPCH2`) for optimized pairwise sidecars over
  whole directory builds ([PAIRWISE_SIDECARS.md](docs/PAIRWISE_SIDECARS.md)).
- Per-file strategy selection across `copy-old`, streaming plan ops,
  `bsdiff`, `xdelta3` and `full-data` — every applicable candidate is
  generated and measured, the smallest payload wins;
  `--explain-strategies` writes the per-file reasoning.
- Auto compression selection between zstd-19 and brotli-9 per payload
  section, each with its own BLAKE3.
- `cavs route-plan` with device profiles (`default`, `low-memory`,
  `slow-network`, `low-disk`) and memory-budget filtering
  ([DELIVERY_PLANNER.md](docs/DELIVERY_PLANNER.md)).
- Hot-pair patch policy (`cavs patch-policy`, TOML config) to avoid
  O(N²) patch generation.
- `cavs publish-dir` for one-pass release publishing (container +
  signature + plan + sidecar + preview).
- Rename/move detection by content: zero-payload metadata in sidecars,
  reported in `cavs preview`.
- Compressed/high-entropy blob detection
  (`preview --detect-compressed-blobs`) and automatic byte-level-delta
  routing for such files in the sidecar optimizer.
- Full-pipeline benchmark suite (`cavs bench butler-full`,
  `cavs bench full-pipeline`) covering benchmarks A–H, with raw outputs
  under [docs/results/v0.8.0/](docs/results/v0.8.0/README.md).
- Journaled staged sidecar apply (`staging → verified → committing →
  committed`, `failed` on aborts) with recovery tests
  (`cavs test apply-recovery`).
- `--memory-budget` on `cavs apply-patch`; error codes
  `CAVS-E-PATCH-CORRUPT`, `CAVS-E-PATCH-INVALID`,
  `CAVS-E-MEMORY-BUDGET-EXCEEDED`, `CAVS-E-BUTLER-REDIFF-FAILED`.

### Changed

- `cavs optimize-patch` defaults to `--algo auto` / `--compression auto`
  and accepts directories; it now emits v2 sidecars (v1 files remain
  applicable via `cavs apply-patch`).
- Workspace version 0.8.0.

## [0.7.0]

The offline toolkit release. CAVS can now sign, preview, diff, apply, verify
and benchmark build updates **locally, with no CAVS server** — the same
verified copy-range + fresh-data reconstruction model the online client uses,
driven from the command line. This release also adds a fair external **butler
offline** benchmark harness and a multi-route benchmark suite so CAVS can be
compared honestly against full downloads, butler and pairwise delta tools.

Measured highlights (128 MiB synthetic builds, seed 5; butler v15.27.0,
xdelta3 3.2.0, bsdiff). On a typical directory-build release the offline
`.cavsplan` update is **2.51 MiB** — half the v0.6 chunk-route wire (5.42 MiB),
matching butler's 2.52 MiB while diffing 2× faster and applying with a
streaming 8 MiB memory budget instead of butler's 35 MiB (and bsdiff's
2.3 GiB). On a shifted artifact (every byte moved) CAVS ships **4.21 KiB** vs
butler's 68 KiB — 16× less — and ties the byte-level tools. The many-version
stream shows the store-once model: 10 versions of a 32 MiB build live in a
**30.6 MiB** content-addressed store that serves *any* jump directly, where
covering every pair with dedicated patches would need 45 of them. Every route
was verified byte-identical.

### Added

- **`cavs preview`** — classify a new build against the previous version's
  `.cavssig` as `NEW` / `MODIFIED` / `DELETED` / `SAME`, estimate the update
  cost per route, and warn when a large modified file looks
  compressed/high-entropy (small source changes cascade across compressed
  output — publish the folder instead). `--changes-only`, `--json`.
- **`cavs diff-plan`** — produce a deterministic, BLAKE3-sealed offline
  reconstruction plan (`.cavsplan`, `cavs-plan` crate): COPY ranges that
  reuse old bytes + INLINE data (zstd-19) for what changed, plus directory
  metadata and managed deletions. `portable` (self-contained patch) or
  `--analysis` (ops + estimates only); diffs against `--old-signature`
  without the old bytes present. Deterministic: same inputs ⇒ same bytes.
- **`cavs apply`** — execute a `.cavsplan` locally. Artifact plans write
  `<out>.part` and rename after a full-hash check; directory plans stage into
  `.cavs-staging/`, verify every file, journal intent, then commit per file.
  An interrupted apply finishes by re-running (or `--resume <journal>`);
  unchanged files are never touched (mtime survives), mods/saves are
  preserved, deletions happen only with `--delete-removed-files`. A failed
  apply never leaves corrupt output.
- **`cavs verify-install`** — verify an installed artifact/directory against a
  `.cavssig` or a manifest's `sha256:` digests, reporting exact
  `MODIFIED`/`MISSING`/`EXTRA` per entry and exiting non-zero on mismatch.
  `--allow-extra-files` tolerates mods and saves.
- **`cavs file` / `cavs ls`** — identify and list any CAVS file (`.cavs`,
  `.cavssig`, `.cavsplan`, `.cavspatch`, manifest, zstd bootstrap); unknown or
  corrupt files fail cleanly. `cavs signature ls` and `--json` on the
  signature commands.
- **`cavs bench butler-offline`** — drive an external `butler` binary through
  its `diff`/`apply`/`verify` pipeline (`-j` JSON lines captured), measure
  wall time and peak RSS, and verify the output byte-for-byte. Labeled as
  butler's **offline/default patch**, explicitly *not* the
  backend-optimized patch. Fails gracefully when butler is absent.
- **`cavs bench pairwise-proxy`** — approximate the optimized pairwise-patch
  class with bsdiff/xdelta3 × zstd/brotli, always labeled a **proxy**, never
  official backend numbers; records tool versions, verifies every apply.
- **`cavs bench routes`** — every delivery route for one transition in one
  table (full downloads, CAVS chunk/hybrid, CAVS offline plan, butler
  offline, pairwise proxies). Missing tools are skipped, not fatal.
- **`cavs bench version-stream`** — many-version storage/served-bytes
  comparison (CAVS store-once vs pairwise patches for adjacent updates, long
  jumps and reinstalls), and **`cavs bench gen-dir`** for synthetic directory
  build pairs (modified/new/deleted/renamed files).
- **Experimental pairwise sidecars** — `cavs optimize-patch` /
  `cavs apply-patch` wrap an external byte-level delta in a verified
  `.cavspatch` for one hot old→new pair (both ends BLAKE3-checked, atomic
  rename). Optional route with an explicit O(N²) warning.

### Changed

- **Directory/container mode is stable** (was a v0.6.0 preview):
  `--ignore <glob>` (repeatable) plus a root `.cavsignore`, path
  normalization and traversal rejection, deterministic sorted packing.
- 13 new stable `CAVS-E-*` codes: `PLAN-CORRUPT`, `PLAN-INVALID`,
  `APPLY-HASH-MISMATCH`, `JOURNAL-CORRUPT`, `JOURNAL-RESUME-FAILED`,
  `PATH-TRAVERSAL`, `UNSUPPORTED-SYMLINK`, `PAIRWISE-TOOL-MISSING`, and the
  `BUTLER-NOT-FOUND` / `-DIFF-FAILED` / `-APPLY-FAILED` / `-VERIFY-FAILED`
  harness codes.
- New crate `cavs-plan`; new docs: `OFFLINE_TOOLKIT.md`,
  `CAVSPLAN_FORMAT.md`, `DIRECTORY_MODE.md`, `ROUTE_BENCHMARKS.md`,
  `BUTLER_COMPARISON.md`, `PAIRWISE_SIDECARS.md`.

### Notes

The butler benchmark measures butler's offline/default patch, not the
backend-optimized distribution patch (bsdiff + high-quality Brotli). The
bsdiff/Brotli results are reported separately and labeled as an optimized
pairwise **proxy**, not official backend numbers. No wire format or routing
changed for existing paths; the v0.5/v0.6 online numbers are unaffected.

## [0.6.0]

The hybrid reconstruction release: CAVS can now use a previously installed
artifact or directory tree as a first-class source of reusable bytes, while
keeping the content-addressed cache, packfile store and verified,
byte-identical reconstruction model intact. It folds the core idea of delta
patching (old-version signatures, copy-range reuse, coalescing, preferred
sources, no-op detection, staged applies) into the CAVS model — as design
ideas, not a rewrite.

Measured highlights (128 MiB synthetic suite, seed 5): a client with an
**empty cache but the old version on disk** updates for 6.24 MiB instead of
64.55 MiB (−90.3 % small change), 26.53 MiB instead of 64.56 MiB (−58.9 %
medium), and 10.9 KiB instead of 64.56 MiB (−99.98 % shifted). The
warm-cache path is byte-for-byte unchanged from v0.5 (no regression), every
output stays byte-identical, and a corrupt previous install demotes to
cache/network per range instead of failing. Range coalescing turns 1,082
per-chunk ops into 18 contiguous reads on the shifted variant. A 128 MiB
build's `.cavssig` signature is 88 KiB (0.07 %). Re-fetching an
already-current install is a no-op: 0 payload bytes.

### Added

- **`--previous-artifact` client mode.** The old install is memory-mapped,
  chunked with the packer's recorded profile and indexed by the new
  manifest's hashes; matched ranges are copied directly — each one
  BLAKE3-verified before writing, with the final SHA-256 gate unchanged. A
  failed range demotes to cache/network and reports
  `CAVS-E-PREVIOUS-ARTIFACT-MISMATCH` (recoverable). The client also
  overrides a server bootstrap suggestion when the previous install makes
  the chunk path cheaper.
- **Hybrid reconstruction plans** (`cavs-rebuild-plan` crate). Every data
  track is rebuilt through a unified, deterministic plan
  (`CopyPreviousRange` / `CopyCacheChunk` / `FetchNetworkChunk`) with
  cost-based source scoring (network ≫ seeks ≫ local reads), contiguity
  preference, and strict adjacent-range coalescing up to 8 MiB per read.
  The v0.5 cache+network flow is expressible as a plan with no
  previous-range ops. `--dump-plan plan.json` exports it; `--no-hybrid`
  restores v0.5 behaviour.
- **CAVS signatures (`.cavssig`)** (`cavs-signature` crate) with
  `cavs signature export|inspect|verify`: a compact (0.07 % of source),
  deterministic description of an old artifact/directory — fixed 64 KiB
  blocks, weak rolling-hash prefilter, BLAKE3-256 strong hashes, per-file
  layout, Merkle root and a whole-file integrity trailer. Exportable from
  `.cavs` containers, raw files or directories.
  `cavs pack --against-signature old.cavssig` reports reusable bytes at
  pack time without the old content. New fuzz target
  (`fuzz_signature_decode`) asserts decode∘encode canonicality.
- **Hybrid diff scanner.** rsync-style weak rolling hash over new bytes,
  candidates confirmed with BLAKE3, `DATA` ops capped at 4 MiB, adjacent
  copies coalesced — finds shifted/unaligned reuse against a signature
  alone (no old bytes, no chunk cache).
- **No-op detection** (default on; `--force-reconstruct` disables): outputs
  that already match cost one manifest round-trip
  (`delivery_mode: "no-op"`); a previous artifact that already *is* the
  target installs by verified local copy (`"previous-copy"`); directory
  updates skip unchanged files (mod-friendly).
- **Directory/container mode (preview).** `cavs pack-dir ./Build -o b.cavs`
  packages a tree as per-file deduplicated tracks plus dir/symlink/exec
  metadata; the client reconstructs into a staging directory, verifies
  every file hash, commits with per-file renames under a journal, and
  optionally `--prune`s files dropped by the new version (unknown files —
  mods, saves — are preserved by default).
- **Delta benchmark baseline.** `cavs bench delta --old A --new B [--out d]`
  measures a block-based delta model (64 KiB blocks, weak rolling hash +
  BLAKE3 confirmation, COPY/DATA planning, zstd-1 transport) against full
  re-download, the CAVS chunk route, and xdelta3/bsdiff when installed —
  patch size, generation and apply times, plus JSON/markdown reports. See
  docs/DELTA_COMPARISON.md for results and honest framing (pairwise patches
  win per-pair bytes; CAVS wins the operational model).
- **Compression benchmark.** `cavs bench compression --input f --algos
  zstd-3,brotli-9` (Brotli feature-gated behind `brotli-bench`). Measured:
  zstd and Brotli within 0.1 % on size, zstd ~40× faster to decode — zstd-3
  stays the default.
- **Static/CDN export plans.** `cavs store export --out dir --static-plans`
  adds per-asset `chunk-map.json` (chunk → pack file, offset, lengths) so a
  client against a static HTTP host can plan fetches with no smart server.
- **Per-source fetch stats.** `--stats-json` now reports
  `sources.{network,cache_chunk,previous_artifact,repair_wire}_bytes`,
  demoted chunk counts, plan op counts before/after coalescing and
  source-selection time.
- **Error taxonomy additions** (stable codes): `CAVS-E-SIGNATURE-CORRUPT`,
  `CAVS-E-SIGNATURE-MISMATCH`, `CAVS-E-PREVIOUS-ARTIFACT-MISSING`,
  `CAVS-E-PREVIOUS-ARTIFACT-MISMATCH`, `CAVS-E-HYBRID-PLAN-INVALID`,
  `CAVS-E-HYBRID-SOURCE-FAILED`, `CAVS-E-CONTAINER-APPLY-FAILED`,
  `CAVS-E-CONTAINER-ROLLBACK-FAILED`, `CAVS-E-DELTA-BENCH-UNAVAILABLE`.

### Changed

- The client reconstruction path for container payloads (raw and directory)
  now goes through the unified plan executor; existing cache/network
  behaviour is preserved as a special case of the plan model (verified: the
  planner never increases network bytes over v0.5 for the same cache
  state). Media payloads keep the v0.5 streaming path.
- Bloom false-positive repair now also consults the previous-artifact index
  before re-fetching a referenced chunk.

### Notes

This release does not replace FastCDC or convert CAVS into a pairwise
patcher. It adds a hybrid source model: cache chunks, previous-artifact
ranges, packfile ranges and network chunks all participate in the same
verified rebuild. New docs: docs/HYBRID_RECONSTRUCTION.md,
docs/SIGNATURE_FORMAT.md, docs/DELTA_COMPARISON.md.

## [0.5.0]

The production-hardening release: correctness under malformed input, recovery
from interrupted downloads and corrupt caches, structured errors, fuzzing,
and large-build confidence. Not about reducing bytes — about trust. Wire
numbers are byte-for-byte identical to 0.4.0 on the real-build suite (tps-demo
update still 1.64 MiB, warm re-fetch still 0 bytes, everything
byte-identical), so all 0.3.0/0.4.0 wins carry over unchanged.

Measured highlights: an interrupted 232 MiB bootstrap download (client
killed with `kill -9` at 57 MiB) resumed with an HTTP Range request and paid
only the missing ~166 MiB; `cache verify` + `cache repair` on a real
5,747-chunk cache detected, quarantined and re-fetched exactly the corrupted
entries; client peak RSS stays ~14 MiB installing a 569 MB payload; the 1 GiB
synthetic suite packs in ~7 s per version, and a head-insertion that shifts
every byte of a 1 GiB build costs 10.9 KiB of update egress (FastCDC
resynchronization working as designed).

### Added

- **Structured error taxonomy.** Stable `CAVS-E-*` codes
  (`cavs_proto::errors`): `MANIFEST-CORRUPT`, `BOOTSTRAP-HASH-MISMATCH`,
  `CHUNK-HASH-MISMATCH`, `CACHE-CORRUPT-RECOVERABLE`, `NETWORK`,
  `OUTPUT-HASH-MISMATCH`, `SIGNATURE-INVALID` and friends — attached at
  every client/CLI failure point, recoverable from any rendered error
  chain, so tooling can decide retry/repair/give-up without parsing prose.
- **Fuzzing.** Five libFuzzer targets under `fuzz/` (manifest v2, varint,
  pack index, container, CVSP batch; `cargo +nightly fuzz run <target>`),
  plus deterministic mini-fuzz replay tests that run in normal CI:
  full byte-flip sweeps, truncation sweeps and seeded random garbage
  against every decoder.
- **Corruption matrix.** `cavs test corrupt <file.cavs> [--out report.json]`
  mutates a scratch copy across ~20 targeted rows — container magic,
  section directory, section bytes, chunk data, truncation, manifest v2
  header/body/truncations, overlong/truncated varints, bootstrap sidecar,
  packfile header/data/footer, pack index, out-of-range reads — and
  asserts every decoder rejects the corruption cleanly.
- **Resume downloads.** A crash-safe journal (`<cache>/journal/…`, written
  tmp+rename) records in-flight fetches. Interrupted bootstrap downloads
  keep their `.zst.part` and continue with an HTTP `Range` request
  (`cavs-server` now answers 206 on `/bootstrap`; older servers get a
  clean restart); interrupted chunk fetches resume naturally from the
  cache have-set. Journals are honoured only when server, asset and
  manifest hash all match — anything stale is discarded with its partial
  files. New `cavs-client resume` command; `--no-resume` opts out. The
  final artifact is still only promoted after full verification.
- **Retry with backoff.** Transient failures (transport errors, 429/5xx)
  retry up to 5 times with exponential backoff (250 ms → 8 s, ±25%
  jitter); verification failures and 4xx never retry. Exhausted retries
  surface as `CAVS-E-NETWORK`.
- **Cache maintenance.** `cavs-client cache verify` re-hashes every cached
  chunk, quarantines corrupt entries (or `--delete`s them) and removes
  torn temp files; `cache repair <server> <asset>` re-fetches exactly the
  missing/corrupt chunks of an asset; `cache gc --max-size 10GiB` evicts
  least-recently-used chunks to a size budget.
- **`cavs doctor`.** Read-only diagnosis: a `.cavs` (structure, every
  chunk hash, Merkle root, manifest encodability in both formats,
  bootstrap sidecar size+BLAKE3, signature, duplicate chunk entries), a
  global store (`--store`: layout, ledger consistency, every chunk, pack
  integrity) and a client cache (`--cache`: corrupt entries). `CAVS-E-*`
  findings, non-zero exit on problems.
- **Large-build benchmark suite.** `cavs bench gen` produces deterministic
  synthetic datasets (same seed ⇒ identical bytes anywhere): a base build
  plus small/medium/large-change, head-shifted and reordered update
  variants, streamed so datasets larger than RAM generate fine.
  `cavs bench suite` packs every version and reports pack time, container
  and manifest sizes, dedup, update egress and packfile shape as
  `summary.md` + `summary.json`.

### Fixed

- **Unbounded pre-allocation in the CVSP decoders** (found by the new
  fuzz targets): a crafted batch header declaring billions of items or a
  multi-GiB inline payload could force huge allocations before the read
  failed. Counts are now capped by what the buffer could actually encode,
  and inline lengths are validated against a 256 MiB wire ceiling before
  allocating.
- **Container reader accepts less.** The superblock's declared hash
  algorithm, compression algorithm and file size are now validated
  (values a correct writer never produces are rejected). The remaining
  superblock fields (uuid, timescale, flags) are intentionally
  unauthenticated metadata — content integrity is carried by the section
  hashes, chunk hashes and Merkle root, as verified by the new full
  byte-flip sweep.

## [0.4.0]

The packfile release: the global store can now keep its chunks in a few
large immutable `.cavspack` files instead of one file per chunk, served by
coalesced range reads. Loose stores keep working unchanged; `.cavs`
file-serving is untouched.

Measured on real Godot games (two versions ingested, full
cold + update + warm HTTP session): chunk objects on disk drop from 130/807/
5,775 (Marble/GDQuest/tps-demo) to **4/4/6 files**, and physical reads
coalesce **65×/115×/170×** with **1.000 read amplification** (zero extra
bytes read — chunks are packed in reconstruction order, so merged ranges are
exactly contiguous). Wire bytes, routing and byte-identical reconstruction
are identical to 0.3.0 in every layout and in `.cavs` file-serving mode.

### Added

- **Packfile storage** (`cavs store add --storage packfiles`). Chunks are
  appended in reconstruction order into content-addressed packs
  (`packs/<ab>/<id>.cavspack`, id = BLAKE3 of the file) with a verifiable
  `.cavsindex` sidecar each. The layout is fixed at store creation; the
  ledger records each chunk's pack and offset. GC deletes a pack once no
  live chunk references it (the roadmap's zero-live-pack policy).
- **Coalesced range serving.** The server plans each batch's cold chunks
  as one read set: chunks from the same pack within a 64 KiB gap are
  fetched with a single physical read (capped at 8 MiB). New metrics:
  `cavs_pack_chunks_requested_total`, `cavs_pack_ranges_read_total`,
  `cavs_pack_bytes_read_total`, `cavs_pack_bytes_served_total`.
- **Manifest chunk-location hints.** Binary v2 manifests of packfile-store
  assets carry an optional ChunkLocations section (section kind 4 —
  skipped by 0.3.0 readers) mapping each chunk to `pack_id + offset +
  stored_len`. Advisory: consumers verify by BLAKE3 regardless.
- **`cavs store export --out`** — deterministic immutable object tree
  (`chunks/packs/…`, `chunks/indexes/…`, `assets/<name>/record.json`)
  ready to upload to S3/R2/a static host behind a CDN, with the cache
  headers to use.
- **`cavs store verify`** — re-hashes every chunk (loose or packed,
  including zstd-stored) and checks pack header/footer integrity.
- **ETag headers** (`"blake3-…"`) on the immutable chunk and bootstrap
  endpoints, complementing the existing immutable Cache-Control.

## [0.3.0]

The compact-manifest release: the runtime manifest now travels as a compact
binary format (`CAVSMF2`) instead of JSON, cutting manifest wire overhead
dramatically while keeping full JSON v1 compatibility for old clients and
servers. Reconstruction, dual-route behavior and warm-cache savings are
unchanged.

Measured on real Godot games (64 KiB CDC, real HTTP sessions): manifests
shrink 75–77% — tps-demo 894 → 209 KiB, GDQuest 103 → 25 KiB, Marble
20 → 5 KiB — with parse time at parity with JSON. Chunk-path bytes are
byte-for-byte identical to 0.1.2 (tps-demo update still 1.64 MiB), warm
re-fetch stays at 0 payload bytes and reconstruction stays byte-identical.
Since the manifest is the dominant cost of an update check, a warm re-fetch
now costs ~75% less wire; total update egress improves up to −26.6%
(tps-demo) depending on how much the manifest weighed.

### Added

- **`cavs-manifest` crate.** One home for manifest wire formats: a strict
  unsigned-LEB128 varint codec, the binary v2 encoder/decoder, and
  `read_manifest`, which detects JSON v1 vs binary v2 from the bytes and
  normalizes both into the same runtime `Manifest` — server, client and CLI
  never branch on formats downstream.
- **Binary manifest v2 (`CAVSMF2`).** Sectioned envelope (AssetInfo,
  ChunkPlan, ChunkDictionary) with per-section BLAKE3 integrity. Chunk
  hashes are stored once, as raw 32-byte BLAKE3, in a dictionary; every
  chunk reference in the plan is a varint dictionary index instead of a
  repeated 64-char hex string. Sections ≥ 32 KiB are zstd-compressed. The
  decoder enforces hard limits (section count/size, decompression ratio,
  string length, overlong varints) so malformed or hostile manifests fail
  cleanly — verified by truncation sweeps and a full byte-flip test.
- **Format negotiation.** `GET /api/assets/{asset}/manifest` serves binary
  v2 when requested via `Accept: application/vnd.cavs.manifest-v2` or
  `?format=binary-v2`; JSON v1 remains the default response, so v0.2.x
  clients work unchanged. New per-format manifest counters in `/metrics`.
- **Client negotiation + manifest metrics.** `cavs-client` asks for binary
  v2 (JSON fallback keeps old servers working) and reports
  `manifest.format/wire_bytes/parse_ms/chunk_count_logical/chunk_count_unique`
  in `--stats-json`.
- **`cavs manifest export`** — readable JSON v1 manifest from a `.cavs`
  (debug/compatibility view).
- **`cavs manifest bench`** — compares JSON v1 vs binary v2 for the same
  container: wire bytes, parse time, bytes per logical chunk, savings; text
  and `--json` output.

## [0.1.2]

The v2 efficiency release: cold installs now cost *less* than downloading the
full compressed release, while updates keep their savings. Measured on real
games (tps-demo, GDQuest, Marble, Godot 4.7 exports): cold install −7% to
−13% vs the zstd-3 full download (previously +2–4% overhead), updates
−81.2% to −99.3%, warm re-fetch 0 bytes, byte-identical reconstruction.

### Added

- **Dual delivery route.** `cavs pack --bootstrap` emits a
  `<output>.bootstrap.zst` sidecar (whole artifact, zstd-19) recorded in the
  container metadata. `cavs-server` verifies it at load, estimates the
  chunk-path payload per session and routes cold clients (<5% cached) to the
  bootstrap when it is ≥2% cheaper; new `GET /api/assets/{asset}/bootstrap`
  endpoint (immutable, streamed) and bootstrap metrics. `cavs-client`
  downloads it streaming, verifies BLAKE3 + SHA-256, installs atomically and
  **seeds its chunk cache** from the manifest chunk plan, so the next update
  is incremental. Chunk path remains the fallback everywhere.
- **Payload classifier.** Format magic + extension + sampled entropy + zstd
  probe decide the candidate chunk profiles: precompressed content gets large
  fixed chunks, engine packs get CDC, text gets small CDC.
- **Chunk-profile auto-sweep.** Six candidate profiles (fixed 256K/512K/1M,
  FastCDC 64K/128K/256K) measured on the real bytes and scored by a weighted
  cost model. `cavs pack --profile auto` applies the cheapest; the new
  `cavs sweep <build> [--prev <old>]` prints the full table (and `--json`).
  `--prev` accepts the previously *published* `.cavs` so reuse is measured
  against real chunk hashes and profile choice stays consistent across a
  version stream.
- Fetch stats now report `delivery_mode`, `seeded_chunks` and `seed_ms`.

### Changed

- Session-open responses now include the delivery-route decision (additive,
  optional fields — fully backward compatible with 0.1.x clients/servers).
- Godot plugin version aligned to 0.1.2 (no behavioural change; the GDScript
  client keeps using the chunk path).

## [0.1.1]

### Fixed

- CI: build all workspace binaries before running tests, so the `cavs-client`
  integration tests can find the `cavs-server` / `cavs` binaries they spawn on a
  clean runner.

### Changed

- Release workflow now publishes all crates to crates.io automatically on a
  version tag (in dependency order), so releases no longer require a manual
  `cargo publish`.

## [0.1.0]

Initial public release.

### Added

- `.cavs` content-addressable container format (FastCDC + zstd + BLAKE3),
  with a global Merkle root, per-file SHA-256, and optional Ed25519 signatures.
- `cavs` CLI: pack / unpack / info / verify / keygen, and a global
  content-addressable store (`store add` / `rm` / `gc` / `stat`) with reference
  counting and garbage collection.
- `cavs-server`: stateful HTTP/HTTPS origin with per-session have-set planning
  (exact or Bloom filter), CVSP binary batches, `--store` mode, HLS passthrough,
  and Prometheus metrics.
- `cavs-client`: native streaming client with a persistent cache and atomic,
  verified reconstruction (resumable, retry-safe).
- Godot 4 plugin: pure-GDScript runtime client that mounts reconstructed packs
  with `load_resource_pack()`.
- `cavs-steam`: SteamPipe update-size analyzer for game builds.
- Documentation: format specification, architecture, benchmarks, and paper.

[Unreleased]: https://github.com/orelvis15/cavs-oss/compare/v0.8.0...HEAD
[0.8.0]: https://github.com/orelvis15/cavs-oss/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/orelvis15/cavs-oss/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/orelvis15/cavs-oss/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/orelvis15/cavs-oss/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/orelvis15/cavs-oss/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/orelvis15/cavs-oss/compare/v0.1.2...v0.3.0
[0.1.2]: https://github.com/orelvis15/cavs-oss/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/orelvis15/cavs-oss/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/orelvis15/cavs-oss/releases/tag/v0.1.0
