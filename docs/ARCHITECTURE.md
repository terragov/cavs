# Architecture

CAVS is a content-addressable delivery layer that sits above your existing
formats. The core idea: split content into chunks identified by their hash,
store each unique chunk once, and transmit only the chunks a client lacks.

```
 build / CI                origin server              client (runtime)
┌────────────────────┐   ┌────────────────────┐   ┌────────────────────────────┐
│ cavs pack          │   │ cavs-server        │   │ native client / Godot / web │
│  build.pck→.cavs   │ → │  .cavs or --store  │ → │  persistent chunk cache     │
│ cavs store add     │   │  session have-set  │   │  fetch only missing chunks  │
│  (dedup at rest)   │   │  inline/ref plan   │   │  verify + atomic rebuild    │
└────────────────────┘   └────────────────────┘   └────────────────────────────┘
```

## Crates (`core/`)

| Crate | Responsibility |
|---|---|
| `cavs-hash` | BLAKE3-256 chunk identity, incremental hasher, binary Merkle root, the content-signature message |
| `cavs-chunker` | Chunking: fixed-size (CDN-aligned segments) and FastCDC (shift-resistant, default 64 KiB for mixed binary payloads) |
| `cavs-store` | In-memory dedup index used while packing, and the **global content-addressable store** (on-disk CAS with reference counting and GC) — loose object-per-chunk, or immutable `.cavspack` packfiles read by coalesced ranges (0.4.0) |
| `cavs-format` | The `.cavs` binary format: types, streaming writer, hardened reader/verifier, Ed25519 signing |
| `cavs-proto` | CVSP wire protocol: the runtime `Manifest` model, sessions, compact binary batches (hardened decoders); the Bloom-filter have-set; the `CAVS-E-*` error taxonomy |
| `cavs-manifest` | Manifest wire formats: the compact binary v2 codec (`CAVSMF2`: chunk dictionary + varint plan, ~76% smaller than JSON) and `read_manifest`, which detects JSON v1 vs binary v2 from the bytes and normalizes both |
| `cavs-signature` | v0.6.0: the compact `.cavssig` old-version signature (fixed-block layout + weak rolling hash + BLAKE3 strong hashes) and the rsync-style hybrid diff scanner that finds reusable ranges against a signature alone |
| `cavs-rebuild-plan` | v0.6.0: the unified reconstruction plan (`CopyPreviousRange` / `CopyCacheChunk` / `FetchNetworkChunk`), cost-based source scoring, and adjacent-range coalescing; the v0.5 cache+network flow is a special case |
| `cavs-plan` | v0.7.0: the `.cavsplan` offline reconstruction plan — deterministic BLAKE3-sealed COPY/INLINE format, the builder that diffs a new build against an old `.cavssig`, and the staged, journaled, mod-friendly apply (artifact and directory modes) |
| `cavs-cli` | The `cavs` binary: pack / pack-dir / unpack / info / verify / keygen / store / play / sweep / signature / doctor / bench, the payload classifier and chunk-profile cost model; v0.7.0 adds the offline toolkit (preview / diff-plan / apply / verify-install / file / ls) and v0.8.0 the delivery planner (route-plan), per-file optimized sidecars (optimize-patch / apply-patch), patch-policy, publish-dir and the full-pipeline + apply-recovery harnesses |
| `cavs-server` | Stateful HTTP/HTTPS origin: sessions, inline/ref planning, `--store` mode, HLS passthrough, HTTP Range on the bootstrap endpoint, metrics |
| `cavs-client` | Native streaming client: persistent cache with verify/repair/gc, `.part`→verify→rename reconstruction, resume journal, retry with backoff, and (v0.6.0) hybrid reconstruction from a `--previous-artifact` with no-op detection and directory-mode staged applies |

The Godot plugin (`game-engine-plugins/godot-plugin/`) is a pure-GDScript client — no native
binary. The SteamPipe-style analysis (v0.9.0) lives inside the `cavs` CLI,
backed by two library crates: `cavs-analyzer` (fixed-1MiB model, pack
diagnostics, recommendations) and `cavs-workspace` (local
app/depot/branch/build metadata). There is no separate steam-analyzer
product (see `WHY_NO_STEAM_ANALYZER_PRODUCT.md`).

## How an update flows

1. **Pack once (publisher).** `--profile auto` classifies the payload (magic
   bytes, sampled entropy, a zstd probe) and measures candidate chunk profiles
   on the real bytes; engine packs default to FastCDC ~64 KiB chunks
   identified by BLAKE3. Chunks are stored deduplicated and zstd-compressed in
   a signable `.cavs`; `--bootstrap` additionally emits the whole artifact as
   one zstd-19 sidecar for cold installs. Unchanged regions across versions
   produce identical chunks (deduplicated for free); changed regions produce
   new chunks. Packing the next version with `--prev <published .cavs>` keeps
   the profile consistent with what clients already cached.

2. **The client announces what it has.** It first fetches the asset manifest —
   negotiated as compact binary v2 (`CAVSMF2`, ~76% smaller than the JSON v1
   equivalent) with JSON as the compatibility fallback — then opens a session
   sending the have-set of its persistent cache: either an exact hash list, or
   a compact Bloom filter for large caches. The server decides per chunk: send
   a *reference* if the client already has it, or *inline* the payload if not.

3. **The server picks the cheapest route (dual delivery).** At session open it
   estimates the chunk-path payload for this specific client. A cold client
   (<5% of chunks cached) whose estimate is beaten by ≥2% by the bootstrap
   artifact is routed to it: one immutable, CDN-cacheable download at
   full-artifact price. Everyone else gets the chunk path. The routing is
   advisory and the chunk path always remains valid. v0.8.0 generalizes
   this idea into a full client-state planner (`cavs route-plan`): no-op,
   chunks, hybrid, offline plan, optimized sidecar, bootstrap or full
   download, scored under device profiles with memory budgets
   ([DELIVERY_PLANNER.md](DELIVERY_PLANNER.md)).

4. **Only the new bytes travel.** Binary CVSP batches carry chunks exactly as
   stored (already compressed — zero recompression), decoded incrementally from
   the socket so peak memory is one chunk. A bootstrap download streams to
   disk the same way.

5. **Atomic, verified reconstruction — and cache seeding.** The client writes
   a `.part` temp file, verifies the manifest's SHA-256 on the fly, then
   atomically renames into place. A bootstrap install is additionally sliced
   along the manifest's chunk plan (each slice BLAKE3-verified) straight into
   the local cache, so the *next* update only pays for changed chunks. An
   interrupted download never leaves a corrupt file, and retrying only fetches
   what's missing.

## Integrity chain

- **Per chunk**: BLAKE3 of the decompressed payload must equal its identity hash.
- **Per section**: BLAKE3 of each table against the section directory.
- **Global**: Merkle root over the chunk table, checked against the INTEGRITY
  section; optionally an Ed25519 content signature the client can pin to a
  trusted publisher key.
- **Per file (thin clients)**: a SHA-256 per reconstructed file, embedded in
  the manifest, so clients without BLAKE3 (e.g. Godot) verify with a built-in
  hasher.

## Storage vs egress dedup

Client-side caching already delivers **egress** savings across versions and
sessions. The **global store** (`cavs store` / `cavs-server --store`) adds
**at-rest** savings: one physical copy of each unique chunk across every
published version and title, with reference counting and garbage collection.

Since 0.4.0 the store can keep those chunks in a few large immutable
**packfiles** (`add --storage packfiles`) instead of one file per chunk:
content-addressed `.cavspack` files written in reconstruction order, served
by range reads that the server coalesces per batch (nearby chunks of one
pack = one physical read). On real builds this turns thousands of chunk
objects into a handful of files and cuts physical reads 65–170× with zero
read amplification — and `cavs store export` emits the store as a
deterministic immutable tree for object storage/CDN (with `--static-plans`
it also writes per-asset `chunk-map.json`, so a client can plan against a
dumb static host). Loose stores keep working unchanged; wire behavior is
identical either way.

## Hybrid reconstruction (v0.6.0)

The previous installed version is a third byte source alongside the cache
and the network. When the client is given `--previous-artifact`, it
memory-maps the old file, chunks it with the packer's recorded profile, and
indexes only the hashes the new manifest needs. Reconstruction then goes
through a single **plan** per output file (`cavs-rebuild-plan`): for each
required chunk a cost model (network bytes ≫ seeks ≫ local reads) picks the
cheapest source, prefers a previous-artifact range that continues the last
one, and coalesces adjacent previous ranges into reads of up to 8 MiB. The
v0.5 cache+network path is exactly a plan with no previous-range ops, so the
executor is a superset, not a parallel path — and the planner is proven
never to increase network bytes over v0.5 for the same cache state.

Trust is unchanged: every copied range re-hashes to its BLAKE3 identity
*before* it is written, the final output still passes the manifest SHA-256
before the atomic promotion, and a range that fails verification demotes to
cache/network per chunk (`CAVS-E-PREVIOUS-ARTIFACT-MISMATCH`, recoverable).
A compact `.cavssig` signature (`cavs-signature`) lets a new version be
diffed against an old one without the old bytes at all, and no-op detection
skips work entirely when an output — or a whole file, in directory mode —
already matches. Directory/container assets (`cavs pack-dir`, preview) are
rebuilt into a staging tree, verified per file, then committed with per-file
renames under a journal. See
[HYBRID_RECONSTRUCTION.md](HYBRID_RECONSTRUCTION.md).

## Failure and recovery (v0.5.0)

Everything above assumes bytes arrive and disks behave. v0.5.0 defines what
happens when they don't:

- **Every write is atomic.** Temp file → verify → rename, for cache chunks,
  reconstructed outputs, manifests and pack indexes alike. An interrupted
  run can leave a `.part` and a journal behind — never a wrong file: the
  final artifact is only promoted after its full digest matches.
- **Downloads resume.** A small crash-safe journal per asset
  (`<cache>/journal/`, written tmp+rename) records the in-flight fetch;
  byte-level truth stays in the artifacts (the `.part` length, the chunk
  cache). Bootstrap downloads continue with an HTTP `Range` request against
  the immutable artifact; chunk fetches re-announce the have-set and pay
  only for what is still missing. A journal is honoured only when server,
  asset and manifest hash all match.
- **Transient ≠ permanent.** Transport errors and 429/5xx retry with
  exponential backoff (250 ms → 8 s, ±25% jitter, 5 attempts); a hash
  mismatch or a 4xx never retries unchanged. Exhausted retries and every
  other failure carry a stable code (`CAVS-E-NETWORK`,
  `CAVS-E-CHUNK-HASH-MISMATCH`, `CAVS-E-CACHE-CORRUPT-RECOVERABLE`, …) so
  launchers can decide programmatically.
- **The cache heals.** Reads always verify (a corrupt entry reads as
  absent); `cache verify` audits the whole cache and quarantines rot,
  `cache repair` re-fetches exactly what an asset is missing, `cache gc`
  evicts LRU to a size budget. Nothing in the cache is trusted twice.
- **Decoders are fuzzed.** Five libFuzzer targets (manifest, varint, pack
  index, container, CVSP batch) under `fuzz/`, plus deterministic
  byte-flip/truncation/garbage sweeps that run in normal CI. The `cavs
  test corrupt` matrix replays ~20 targeted mutations against real
  containers, and `cavs doctor` runs the read-only health checks in
  production.

## Design stance

CAVS is complementary to codecs, compressors and delta tools — not a
replacement. Dedicated pairwise deltas (xdelta, bsdiff) win on raw bytes for a
single version pair; serving *every* jump as one direct patch requires an
all-pairs O(N²) graph, so real systems deploy practical policies instead —
adjacent chains, sparse ladders, base-version hubs, hot pairs — each trading
storage for chain length and apply cost
([`PRACTICAL_PAIRWISE_DIFFS.md`](PRACTICAL_PAIRWISE_DIFFS.md)). Those
policies also cost heavy RAM for large files and don't provide resumable,
CDN-cacheable, cross-version reuse. CAVS packages once per release, the same
chunk store serves any version jump, and `cavs bench patch-policy` measures
the practical policies head-to-head instead of dismissing them. See
[`BENCHMARKS.md`](BENCHMARKS.md).
