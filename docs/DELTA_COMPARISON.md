# CAVS vs delta patching — measured comparison

The established way to ship an update in fewer bytes is a **pairwise
delta patch**: diff the old version against the new one and send only the
difference. Tools like `xdelta3` and `bsdiff` do this at the byte level;
rsync-style tools do it over fixed blocks (a weak rolling hash finds
unchanged blocks, unmatched regions travel as fresh data). They are
excellent at the one thing they do — the smallest possible bytes for **one
specific old→new jump** — and CAVS does not try to beat them at it.

CAVS is a different shape: a **version-stream delivery layer**. Each release
is packaged once into content-addressed chunks, and any client — from any
older version, with any cache state — converges on the newest bytes. v0.6.0
brings the best idea of delta patchers *into* that model: reuse bytes from
the version already installed on disk (see
[HYBRID_RECONSTRUCTION.md](HYBRID_RECONSTRUCTION.md)), without giving up
content-addressing, dedup, resume, repair or CDN-cacheability.

## Methodology — read this first

`cavs bench delta` measures a **block-based delta model** built into CAVS:
fixed 64 KiB blocks, a weak rolling-hash prefilter, strong-hash (BLAKE3-256)
confirmation, and COPY/DATA planning with range coalescing; DATA payloads
compressed with zstd-1. `xdelta3 -9` and `bsdiff` run as byte-level
baselines when present on PATH. Every reconstruction is verified
byte-identical before its size is reported. Reproduce with:

```bash
cavs bench gen --out ds --size 128MiB --seed 5
cavs bench delta --old ds/v1.bin --new ds/v2-small.bin --out results/delta-small
```

## Patch size (128 MiB synthetic suite, seed 5)

| Pair | Full re-download (zstd-19) | Block-delta patch | xdelta3 -9 | bsdiff | CAVS update (chunks) |
|---|---:|---:|---:|---:|---:|
| small change  | 64.05 MiB | 1.94 MiB | 1.94 MiB | 1.96 MiB | 6.06 MiB |
| medium change | 64.05 MiB | 8.83 MiB | 8.82 MiB | 8.90 MiB | 26.28 MiB |
| shifted (every byte moves) | 64.06 MiB | 4.04 KiB | 4.65 KiB | 4.67 KiB | 10.90 KiB |

Generation: block-delta 234–525 ms, CAVS 243–253 ms.
Apply: block-delta 136–160 ms, CAVS 58–62 ms. The block-delta signature
(exchanged once, reusable for any diff against that version) is 88 KiB.

**Honest reading: pairwise patches win per-pair bytes.** A diff that ships
only the dirty regions of dirty blocks, compressed as one stream, beats
shipping whole content-addressed chunks — on this suite by ~3× for
scattered small edits. That is inherent, not an implementation gap: chunk
granularity is what buys CAVS its operational properties. Where the byte
gap matters most (clients updating over slow links, a single artifact, adjacent
versions), a good delta patcher is genuinely strong.

## What the per-pair number does not capture

| Property | Pairwise delta (block-delta / xdelta3 / bsdiff) | CAVS |
|---|---|---|
| Packaging work per release | one patch **per old→new pair** (or a patch chain) | package **once**; all jumps served |
| v1→v5 direct jump | needs that exact patch, or applies 4 chained patches | same chunk fetch as any update |
| Storage across N versions | O(N²) for all-pairs one-hop; practical policies store O(N) adjacent, <2N ladder or budgeted hot pairs and chain the rest ([PRACTICAL_PAIRWISE_DIFFS.md](PRACTICAL_PAIRWISE_DIFFS.md)) | one deduplicated chunk store |
| CDN cacheability | per-pair patch files, per-audience | immutable content-addressed chunks/packs shared by every jump |
| Partial/corrupt local state | patch applies to a pristine old file or fails | cache verify/repair; corrupt ranges demote and re-fetch |
| Resume | restart patch download (tool-dependent) | resumable by design (chunks + HTTP ranges) |
| Byte-identical guarantee | final hash check (tool-dependent) | per-chunk BLAKE3 + final SHA-256, always |

And the v0.6.0 headline: with **no cache at all but the old version on
disk** — the situation a pairwise patcher assumes — CAVS now pays close to
patch-size bytes instead of a full re-download:

| Scenario (small change) | wire bytes |
|---|---:|
| CAVS v0.5, cold cache | 64.55 MiB |
| **CAVS v0.6, cold cache + previous install** | **6.24 MiB (−90.3 %)** |
| CAVS warm cache (v0.5 = v0.6) | 6.24 MiB |
| Block-delta patch (pairwise, pre-generated) | 1.94 MiB |

## Compression: zstd vs Brotli

CAVS ships zstd-3. Brotli is a common alternative for patch/asset transport,
so `cavs bench compression` cross-checks it (32 MiB sample, brotli feature
enabled):

| algo | size | ratio | encode ms | decode ms |
|---|---:|---:|---:|---:|
| zstd-1  | 16.02 MiB | 0.501 | 6    | 4 |
| zstd-3  | 16.02 MiB | 0.501 | 14   | 1 |
| zstd-9  | 16.01 MiB | 0.500 | 19   | 2 |
| zstd-19 | 16.01 MiB | 0.500 | 1216 | 1 |
| brotli-1 | 16.02 MiB | 0.501 | 52  | 77 |
| brotli-9 | 16.01 MiB | 0.500 | 1113 | 77 |

Identical sizes within 0.1 %, zstd decodes ~40× faster here. **zstd-3 stays
the default**; Brotli remains available behind `--features brotli-bench`
for payload classes where it might win (rerun on your own builds).

## Framing

CAVS v0.6.0 combines content-addressed storage with old-version range
reuse. It does not claim to beat a pairwise patcher at its own single-pair
specialty — the numbers above say so explicitly — but it delivers most of the
byte win while keeping the operational properties a per-pair patch cannot.
