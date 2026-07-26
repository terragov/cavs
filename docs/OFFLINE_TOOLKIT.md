# Offline Toolkit (v0.7.0, extended in v0.8.0)

CAVS v0.7.0 turns the delivery system into a complete **local toolkit**:
sign, preview, diff, apply, verify and benchmark build updates with
no CAVS server involved. The same reconstruction model the online client
uses (verified copy-ranges + fresh data) drives every offline command.
v0.8.0 adds the route planner, per-file optimized sidecars, the hot-pair
policy and one-command publishing on top of it.

```bash
# 1. Describe the released version (once, at release time):
cavs signature export ./Build_v1 --raw -o build_v1.cavssig

# 2. See what the next build changes before publishing anything:
cavs preview ./Build_v2 --against build_v1.cavssig --changes-only

# 3. Produce a deterministic offline update plan (a portable patch):
cavs diff-plan ./Build_v1 ./Build_v2 -o update.cavsplan --report plan.md

# 4. Apply it — staged, journaled, verified, mod-friendly:
cavs apply --old ./InstalledGame --plan update.cavsplan --inplace --verify

# 5. Check any install against a known-good state:
cavs verify-install ./InstalledGame --signature build_v2.cavssig --allow-extra-files

# 6. Identify and inspect any CAVS file:
cavs file update.cavsplan
cavs ls build_v1.cavssig

# 7. v0.8.0 — publish a release in one pass (container + signature +
#    plan + optimized sidecar + preview with rename detection):
cavs publish-dir ./Build_v2 --previous ./Build_v1 --out-dir releases/

# 8. v0.8.0 — pick the delivery route for one client state:
cavs route-plan --installed ./InstalledGame --new ./Build_v2 \
  --profile low-memory

# 9. v0.8.0 — prove interrupted applies never break an install:
cavs test apply-recovery --old ./Build_v1 --new ./Build_v2
```

Every command supports `--json` where output is meant to be consumed
programmatically.

## `cavs preview`

Classifies every entry of the new build against the old `.cavssig` as
`NEW` / `MODIFIED` / `DELETED` / `SAME`, estimates the update cost per
route, and warns when a large modified file looks compressed/high-entropy
(small source changes cascade across compressed output — publish the
uncompressed folder instead).

v0.8.0: renamed/moved files are detected from the signature alone (same
size + block hash sequence) and reported as `renamed from … — no
payload`; `--detect-compressed-blobs` additionally flags archives by
magic bytes (zip, gzip, zstd, 7z, xz, bzip2, rar) and quantifies what an
update through the blob costs.

```text
MODIFIED    19.25 MiB  data.pack
NEW        320.00 KiB  assets/asset_40.dat
DELETED          0 B   assets/asset_07.dat

Summary:
  estimated CAVS update    : 717.31 KiB (fresh 1.58 MiB, block-level reuse 32.19 MiB)
  estimated full zstd-3    : 15.59 MiB
```

## `cavs diff-plan` and the `.cavsplan`

A plan is a deterministic, BLAKE3-sealed description of how to rebuild the
new build from the old one: COPY ranges that reuse old bytes, INLINE data
(zstd-19) for what changed, plus directory metadata (created dirs,
symlinks, executable bits, managed deletions). Two kinds:

- **portable** (default): carries the inline payload — a self-contained
  offline patch;
- **analysis** (`--analysis`): ops and estimates only, for reports and CI
  size gates.

`--old-signature build_v1.cavssig` diffs without the old bytes present —
only the new build and the previous release's signature are needed.
Format details: [CAVSPLAN_FORMAT.md](CAVSPLAN_FORMAT.md).

## `cavs apply`

- **Artifact plans**: write `<out>.part`, verify the full BLAKE3, then
  atomically rename. A wrong or corrupted old artifact aborts with
  `CAVS-E-APPLY-HASH-MISMATCH` and leaves nothing behind.
- **Directory plans**: reconstruct changed files into
  `<out>/.cavs-staging/`, verify every file hash *before* commit, write a
  journal (`.cavs-journal.json`), then commit with per-file renames.
  An interrupted apply is finished by re-running the same command (or
  `cavs apply --resume <journal>`); a journal from a *different* apply
  blocks with `CAVS-E-JOURNAL-RESUME-FAILED` instead of guessing.

Mod-friendly by default:

- files whose hash already matches are **never touched** (mtime survives);
- files the plan does not manage (mods, saves) are **preserved**;
- managed deletions happen only with `--delete-removed-files`.

## `cavs verify-install`

Verifies an installed artifact or directory against a `.cavssig` (block
hashes) or a manifest (`sha256:` digests), reporting the exact mismatch
type per entry — `MODIFIED` / `MISSING` / `EXTRA` — and exiting non-zero
on failure. `--allow-extra-files` tolerates mods and saves.

## `cavs file` / `cavs ls`

Identify any CAVS file by magic — `.cavs` containers, `.cavssig`
signatures, `.cavsplan` plans, `.cavspatch` sidecars, manifests, zstd
bootstraps — and list what is inside. Unknown or corrupt files fail
cleanly with a non-zero exit.

## Measured (128 MiB synthetic builds, seed 5)

| Update | Full zstd-19 | CAVS `.cavsplan` | Apply time |
|---|---:|---:|---:|
| directory build, typical change | 62.12 MiB | **2.51 MiB** | 264 ms |
| single artifact, small change | 64.05 MiB | **1.94 MiB** | 95 ms |
| shifted artifact (all bytes moved) | 64.06 MiB | **4.21 KiB** | 94 ms |

Full route comparisons (including butler and bsdiff/xdelta3):
[ROUTE_BENCHMARKS.md](ROUTE_BENCHMARKS.md) and
[BUTLER_COMPARISON.md](BUTLER_COMPARISON.md).

## Apply safety (v0.8.0 additions)

The directory apply journals its state from the first byte staged —
`staging → verified → committing → committed`, with `failed` recorded on
any aborted run — and `cavs test apply-recovery` proves the model on a
real pair: it SIGKILLs live `cavs apply` subprocesses at ramping delays,
asserts that no managed file is ever a torn mix of old and new bytes,
that user files (mods) survive, and that re-running finishes the update;
plus corrupt-plan, corrupt-old-install and garbage-staging cases.

## Error taxonomy additions (v0.7.0 / v0.8.0)

| Code | Meaning |
|---|---|
| `CAVS-E-PLAN-CORRUPT` | `.cavsplan` unparseable / integrity failure |
| `CAVS-E-PLAN-INVALID` | plan parsed but is internally inconsistent |
| `CAVS-E-PATCH-CORRUPT` | `.cavspatch` unparseable / integrity failure |
| `CAVS-E-PATCH-INVALID` | sidecar parsed but is internally inconsistent |
| `CAVS-E-APPLY-HASH-MISMATCH` | output hash wrong; nothing committed |
| `CAVS-E-MEMORY-BUDGET-EXCEEDED` | apply would exceed `--memory-budget` |
| `CAVS-E-JOURNAL-CORRUPT` | apply journal unreadable |
| `CAVS-E-JOURNAL-RESUME-FAILED` | journal belongs to a different apply |
| `CAVS-E-PATH-TRAVERSAL` | container path escapes its root |
| `CAVS-E-UNSUPPORTED-SYMLINK` | symlink not representable on this platform |
| `CAVS-E-PAIRWISE-TOOL-MISSING` | bsdiff/xdelta3/brotli not available |
| `CAVS-E-BUTLER-NOT-FOUND` / `-DIFF-FAILED` / `-REDIFF-FAILED` / `-APPLY-FAILED` / `-VERIFY-FAILED` | external butler benchmark harness |
