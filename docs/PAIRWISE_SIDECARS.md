# Optimized pairwise sidecars (`.cavspatch` v2, v0.8.0)

For a *hot* old→new pair — "previous release → latest" for most clients —
a dedicated patch can beat chunked delivery on wire bytes. Sidecars make
that an **optional route inside CAVS** without changing the architecture:
content stays content-addressed; a sidecar is just a cheaper edge for one
specific version jump.

v2 works on whole directory builds and picks the best strategy **per
file** by measuring real candidate sizes — not by always running one
algorithm:

| Strategy | Payload | Wins on | Apply memory |
|---|---|---|---|
| `copy-old` | none | unchanged + renamed/moved files | streaming |
| `plan-ops` | inline data + copy ranges | shifted/insert-heavy binaries | streaming (~8 MiB reads) |
| `bsdiff` | external byte delta | small binary mutations | old + new + patch in RAM |
| `xdelta3` | external byte delta | compressed/high-entropy blobs | windowed |
| `full-data` | recompressed file | new files | streaming |

Every candidate that applies is generated and measured; the smallest
payload is kept (ties break toward the lower-memory strategy). Renames
are detected by content hash and ship as **zero-payload metadata**.

```bash
# Generate (per-file auto selection; bsdiff/xdelta3 used when on PATH):
cavs optimize-patch --old ./Build_v1 --new ./Build_v2 \
  --algo auto --compression auto \
  --explain-strategies strategies.md \
  -o patches/v1_to_v2.cavspatch

# Apply — staged, journaled, every hash verified before commit:
cavs apply-patch --old ./InstalledGame --patch patches/v1_to_v2.cavspatch \
  -o ./InstalledGame --delete-removed-files

# Refuse strategies that exceed a device's memory budget:
cavs apply-patch ... --memory-budget 128MiB
```

`--explain-strategies` writes a per-file report: size, detected shape
(archive/high-entropy/plain), block-level reuse, every candidate's
measured bytes and why the winner won.

## Format

`CAVSPCH2` magic; old and new entry tables (paths, sizes, full BLAKE3
per file, exec bits, symlinks); one strategy per new file; managed
deletions; independently compressed payload sections (zstd-19 or
brotli-9, whichever is smaller under `--compression auto`) each with its
own BLAKE3; an integrity trailer over the whole file. Strict LEB128
varints, capped counts, path-traversal rejection — the same decoding
discipline as `.cavssig` and `.cavsplan`.

Apply refuses the wrong old version (`CAVS-E-APPLY-HASH-MISMATCH`),
verifies every reconstructed file before the commit phase, journals its
state (`staging → verified → committing → committed`), and a corrupt
sidecar fails at decode (`CAVS-E-PATCH-CORRUPT`). v1 sidecars
(`CAVSPCH1`, whole-artifact) remain applicable.

## Memory budgets

bsdiff's apply holds roughly *old + new + patch* in memory; that is the
price of its small patches. The sidecar records enough to estimate its
peak apply memory up front, so a constrained client can refuse it:

```text
$ cavs apply-patch ... --memory-budget 128MiB
CAVS-E-MEMORY-BUDGET-EXCEEDED: estimated peak 391 MiB exceeds budget
128 MiB — use the .cavsplan route (streaming, ~40 MiB) or raise
--memory-budget
```

The [delivery planner](DELIVERY_PLANNER.md) does this automatically: on a
`low-memory` profile a bsdiff-heavy sidecar is excluded and the plan
route wins even when it costs a few percent more bytes.

## The O(N²) rule: hot pairs only

A sidecar serves **exactly one pair**. With N published versions there
are N·(N−1)/2 pairs — 45 at 10 versions, 4,950 at 100. That count only
applies to the *all-pairs one-hop* graph; practical pairwise systems use
adjacent diffs, sparse ladders or base-version policies instead
([PRACTICAL_PAIRWISE_DIFFS.md](PRACTICAL_PAIRWISE_DIFFS.md)). CAVS
doesn't need any of those graphs — the content-addressed store already
serves *any* jump — so sidecars stay an optimization for hot pairs.
`cavs patch-policy` decides the few pairs worth optimizing:

```toml
[optimized_patches]
enabled = true
max_pairs_per_release = 3
max_total_patch_storage_ratio = 0.25
pairs = ["previous", "latest-stable", "top-installed"]
algorithm = "auto"
compression = "auto"
expire_after_days = 90
```

```bash
cavs patch-policy --versions v1,v2,...,v10 \
  --distribution installed-shares.json --config cavs-patches.toml
```

`previous` covers the adjacent update; `latest-stable` covers the slow
channel; `top-installed` reads a version→share map so the biggest installed
populations get a dedicated patch. Explicit pins (`"v3->v10"`) are
honored first. Every version pair the policy does *not* cover is still
served by chunks/hybrid/plan — no missing routes.

## Measured

See [ROUTE_BENCHMARKS.md](ROUTE_BENCHMARKS.md) for the current full
tables, including the v0.8.0 full-pipeline runs where the per-file
sidecar competes against externally generated optimized patches.
