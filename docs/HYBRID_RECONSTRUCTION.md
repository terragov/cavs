# Hybrid Reconstruction (v0.6.0)

CAVS v0.6.0 makes the **previous installed version a first-class source of
reusable bytes**. Reconstruction no longer assumes the best reusable unit is
a chunk in the cache: a client can copy verified ranges directly from the
old artifact (or old directory tree) that is already on disk, and only fetch
what actually changed — even when its chunk cache is empty.

```text
CAVS v0.5 reconstructed from:        CAVS v0.6 reconstructs from:
  - cache chunks                       - cache chunks
  - network chunks                     - network chunks
  - bootstrap artifact                 - bootstrap artifact
  - packfile store                     - packfile store
                                       - previous installed artifact ranges
                                       - previous installed directory files
```

This borrows the core idea of delta patchers — copy the ranges you already
have, ship only the difference — and folds it into the CAVS model
(old-version signatures, copy-range reuse, coalescing, preferred sources,
no-op detection, staged applies) without replacing the architecture:
content stays content-addressed, cache-first, byte-identical and
CDN-friendly. See [DELTA_COMPARISON.md](DELTA_COMPARISON.md) for measured
comparisons against delta tools and
[SIGNATURE_FORMAT.md](SIGNATURE_FORMAT.md) for the `.cavssig` format.

## Usage

```bash
# Update with the old version installed but an empty (or partial) cache:
cavs-client fetch http://server:8990 game_v2 \
  -o ./install \
  --cache ./cache \
  --previous-artifact ./install/game_v1.pck \
  --stats-json stats.json

# Inspect what the planner decided:
cavs-client fetch ... --dump-plan plan.json

# Old behaviour (cache + network only):
cavs-client fetch ... --no-hybrid

# Disable no-op shortcuts:
cavs-client fetch ... --force-reconstruct
```

## How it works

1. **Index.** The previous artifact is memory-mapped and chunked with the
   *same profile the packer used for the new version* (recorded in the
   manifest meta), so shared content produces identical chunk hashes. Only
   hashes the new manifest needs are kept.
2. **Announce.** Previous-artifact matches join the cache have-set, so the
   server sends references instead of payloads for them — the wire cost is
   the changed chunks only.
3. **Plan.** Each data track gets a `ReconstructionPlan`
   (`cavs-rebuild-plan` crate): for every output chunk the planner picks
   the cheapest source (network bytes ≫ seeks ≫ local reads), preferring a
   previous-artifact range that *continues the last one*, then any previous
   range, then the cache. Adjacent previous ranges coalesce up to 8 MiB per
   read. The v0.5 flow is exactly a plan with no previous-range ops, so the
   executor is a superset, not a parallel path.
4. **Execute + verify.** Every copied range re-hashes to its expected
   BLAKE3 *before* being written; the final output must still pass the
   manifest's SHA-256 before the atomic `.part` → final rename. A range
   that fails verification demotes to cache/network transparently and is
   reported as `CAVS-E-PREVIOUS-ARTIFACT-MISMATCH` (recoverable).

The previous artifact is never trusted blindly — a corrupt old install can
slow the update down, but can never corrupt the output.

## No-op detection

Enabled by default (`--force-reconstruct` disables):

- **Level 1 — output no-op:** every output file already matches its
  manifest digest → the fetch ends after the manifest round-trip
  (`delivery_mode: "no-op"`, zero payload).
- **Level 2 — previous no-op:** the previous artifact already *is* the new
  version → installed with a local verified copy
  (`delivery_mode: "previous-copy"`, zero network).
- **Level 3 — per-file no-op (directory mode):** unchanged files are not
  rewritten; files the user modified and the publisher did not touch are
  left alone.

## Directory / container mode (preview)

Builds distributed as a directory tree rather than a single archive:

```bash
cavs pack-dir ./Build_v1 -o build_v1.cavs
cavs-client fetch http://server:8990 build_v1 -o ./InstalledGame --cache ./cache
```

Each file becomes a deduplicated data track named by its relative path;
empty directories, symlinks and executable bits travel as metadata. Updates
reconstruct changed files into a staging directory
(`.cavs-staging/`), verify every hash, then commit with per-file renames; a
journal records intent so an interrupted apply is finished (or cleaned) by
simply re-running the fetch. `--prune` removes files dropped by the new
version; without it unknown files (mods, saves) are preserved.

Preview limitations: permissions are reduced to an executable bit, symlinks
are recreated on Unix only, hardlinks are not detected.

## Stats

`--stats-json` now includes per-source accounting and plan metrics:

```json
{
  "sources": {
    "network_bytes": 6544479,
    "cache_chunk_bytes": 13243060,
    "previous_artifact_bytes": 120974668,
    "repair_wire_bytes": 0,
    "demoted_chunks": 0
  },
  "reconstruction_plan": {
    "ops_before_coalescing": 1087,
    "ops_total": 154,
    "coalesced_ops": 933,
    "copy_previous_range_ops": 987,
    "source_selection_ms": 1.6
  },
  "no_op": false, "no_op_files": 0, "no_op_bytes": 0
}
```

## Measured results (128 MiB synthetic suite, seed 5, fastcdc-64k)

Cold cache = fresh client, nothing cached. "cold + previous" is the new
v0.6.0 capability: only the old install on disk.

| Update variant | v0.5 cold cache | v0.6 cold + previous | Reduction | Warm cache (both) |
|---|---:|---:|---:|---:|
| small change   | 64.55 MiB | **6.24 MiB**  | **−90.3 %** | 6.24 MiB |
| medium change  | 64.56 MiB | **26.53 MiB** | **−58.9 %** | 26.53 MiB |
| shifted (all bytes moved) | 64.56 MiB | **10.9 KiB** | **−99.98 %** | 10.9 KiB |

- The warm-cache path is byte-identical to v0.5 (no regression; AC verified
  by the planner test "never increases network bytes for the same cache
  state").
- Range coalescing: 1082 → 18 ops on the shifted variant (60×), 1087 → 154
  on small (7×), read amplification 1.0 (strict contiguous merging).
- No-op re-fetch: 0 payload bytes, ~0.4 s wall (manifest round-trip plus
  one streaming SHA-256 of the local file).
- Every scenario ends byte-identical (`cmp` against the source build).

## Error taxonomy additions

| Code | Meaning | Recoverable |
|---|---|---|
| `CAVS-E-SIGNATURE-CORRUPT` | `.cavssig` unparseable / integrity failure | no (for that file) |
| `CAVS-E-SIGNATURE-MISMATCH` | signature does not describe the source | no |
| `CAVS-E-PREVIOUS-ARTIFACT-MISSING` | `--previous-artifact` not found | yes (continue without) |
| `CAVS-E-PREVIOUS-ARTIFACT-MISMATCH` | a range failed verification | yes (demote to cache/network) |
| `CAVS-E-HYBRID-PLAN-INVALID` | plan has output gaps/overlaps | no (falls back to chunk path) |
| `CAVS-E-HYBRID-SOURCE-FAILED` | a source failed and no fallback succeeded | no |
| `CAVS-E-CONTAINER-APPLY-FAILED` | directory apply failed (old install intact) | rerun fetch |
| `CAVS-E-CONTAINER-ROLLBACK-FAILED` | rollback could not restore state | manual |
| `CAVS-E-DELTA-BENCH-UNAVAILABLE` | external tool missing for `bench delta` | n/a |
