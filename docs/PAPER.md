# CAVS-1: Content-Addressable Verified Streaming for Efficient Large-File Updates

**Author:** Orelvis Lago / CAVS Research Draft  
**Date:** 2026-07-04  
**Status:** Technical paper draft for presentation, partner outreach, and patent preparation  

> **Important:** This is a technical research and product paper, not legal advice. Patent-related language should be reviewed by a registered patent attorney before filing.

---

## Abstract

Many systems distribute large binary payloads that change only partially between versions: application and machine images, AI model weights and checkpoints, datasets, container and disk images, media bundles, scientific data, backups, and engine asset packs (Godot PCK files, Unity AssetBundles, Unreal PAK/UCAS containers). Conventional distribution often sends an entire updated package, while pairwise delta tools such as xdelta and bsdiff can produce very small patches but require per-version-pair generation and operational management. This paper presents **CAVS-1**, a content-addressable verified streaming layer that encodes files and bundles into reusable chunks, publishes signed reconstruction manifests, transmits only missing chunks to a client-side cache, and reconstructs target files via constant-memory streaming to disk.

CAVS-1 is not a replacement for video codecs, file compression, or existing distribution platforms. It is a complementary delivery and update layer designed for versioned large files, repeated content, incremental patches, content delivery networks, and runtime asset systems. The benchmark corpus is real Godot projects exported as PCK files — chosen because they are public, reproducible and representative of large mixed-content binary payloads — and showed update payload reductions of 51.9% to 98.0% versus downloading the full updated container compressed with zstd. Comparative tests showed xdelta3 and bsdiff can beat CAVS in pure byte count for a single v1-to-v2 pair, but CAVS serves arbitrary version jumps without maintaining a pairwise patch graph (all-pairs one-hop coverage is O(N^2); practical adjacent, ladder, base-version and hot-pair policies trade storage for patch chains), and supports resumable chunk fetches, cross-version cache reuse, and constant-memory verified reconstruction.

---

## 1. Introduction

Updates to large payloads often modify only a small portion of the installed content. However, because producers commonly pack many files into large containers (archives, image layers, checkpoint files, asset packs), a small edit can cause large binary changes in the surrounding container. This can produce oversized downloads, high local disk I/O, longer update times, and user frustration.

The core research question is:

> Can a delivery layer use content-addressable chunks, a client cache, and verified streaming reconstruction to reduce large-file update downloads without replacing the producer's existing formats, tooling or codecs?

CAVS-1 answers this by separating three concerns:

1. **Encoding:** split assets into reusable chunks and store them by content hash.
2. **Transport:** transmit only chunks the client does not already have.
3. **Reconstruction:** rebuild the target file byte-for-byte from the verified cache using a signed manifest.

---

## 2. Problem Statement

A publisher may ship:

```text
Release v1: main container = 569 MiB
Release v2: main container = 569 MiB with a few files changed
```

A naive update sends the full v2 package. A pairwise delta tool sends only a binary patch from v1 to v2. CAVS takes a different approach:

```text
v1 cache:   chunks A B C D E F G
v2 manifest: chunks A B C X Y F G
missing:    X Y
```

The client downloads only `X` and `Y`, verifies them, stores them in a content-addressed cache, and reconstructs v2.

The design goal is not to beat optimal pairwise binary deltas for a single update pair. The design goal is to scale across:

- many versions alive at once;
- interrupted downloads;
- clients jumping from v1 to v5;
- products with optional or add-on content branches;
- shared content across packages;
- installers, launchers and CDNs;
- host runtimes that mount content packs at runtime;
- CI/CD pipelines that need measurable update budgets.

---

## 3. Related Work

### 3.1 Deduplication and Content-Addressable Storage

Deduplication systems eliminate repeated data at the chunk level. Content-addressable storage identifies chunks by hash rather than by path or location. These concepts are mature and appear in backup systems, object stores, Git, IPFS, CAS systems, and container registries.

CAVS builds on this field but narrows the product target to verified distribution of large binary payloads and update reconstruction.

### 3.2 Rsync and Delta Encoding

Rsync minimizes transfer when the receiver already has a related file. It uses rolling checksums to locate matching blocks. Pairwise delta tools such as xdelta and bsdiff can generate very small patches for a known old-to-new pair. These approaches are strong baselines.

CAVS differs operationally: it packages each release once into a chunk store and can update from any cached subset without generating a separate patch for every source/target pair.

### 3.3 Git and Packfiles

Git stores content-addressed objects and uses packfiles with delta compression. Git is optimized for source control and object history, not direct runtime delivery of large binaries to end clients. CAVS borrows the idea of content-addressed objects but focuses on binary asset distribution, streaming reconstruction, and client cache negotiation.

### 3.4 IPFS and Merkle DAGs

IPFS provides content-addressed block storage and content-addressed links. CAVS is not a peer-to-peer file system. It uses a managed publisher/server/client model intended for application and machine builds, models and datasets, media bundles, launchers, and enterprise distribution.

### 3.5 Content-Defined Chunking and FastCDC

Content-defined chunking cuts data based on content rather than fixed offsets. This helps when insertions or deletions shift file positions. CAVS uses CDC-style chunking, with a tested large-binary-distribution default of min/avg/max 16/64/256 KiB and zstd level 3 for batch/chunk compression.

### 3.6 SteamPipe and Game Pack Files

SteamPipe splits files into roughly 1 MB chunks, compresses and encrypts chunks, and searches for matching chunks in prior builds. Steam documentation warns that pack files, asset reordering, distributed TOCs, absolute offsets, and compression across asset boundaries can cause oversized updates. CAVS is positioned either as an external delivery layer or as an analyzer that helps developers diagnose those risks before publishing.

---

## 4. CAVS-1 Architecture

```text
          Publisher / CI
               |
               v
        +-------------+
        | CAVS Encoder|
        +------+------+ 
               |
      +--------+---------+
      |                  |
      v                  v
+------------+     +-------------+
| Chunk Store|     | Manifest    |
| hash->data |     | signed spec |
+------------+     +-------------+
      |                  |
      +--------+---------+
               |
               v
        +-------------+
        | CVSP Server |
        +------+------+ 
               |
         missing chunks
               |
               v
        +-------------+
        | CAVS Client |
        +------+------+ 
               |
               v
       cache + .part + verify + atomic rename
```

### 4.1 Encoder

The encoder receives a file or directory and produces:

- content-defined chunks;
- content hashes;
- optional compressed chunk payloads;
- a manifest describing file reconstruction order;
- a final file hash;
- optional signature metadata.

### 4.2 Manifest

A manifest is the authoritative reconstruction recipe. It should contain:

```json
{
  "format": "CAVS-1",
  "asset_id": "app-main-pack",
  "version": "1.6.1",
  "chunker": {"mode": "cdc", "min": 16384, "avg": 65536, "max": 262144},
  "compression": {"algorithm": "zstd", "level": 3},
  "file_size": 596796000,
  "file_sha256": "...",
  "chunks": [
    {"index": 0, "hash": "...", "offset": 0, "length": 65536},
    {"index": 1, "hash": "...", "offset": 65536, "length": 64102}
  ],
  "signature": {"algorithm": "Ed25519", "value": "..."}
}
```

### 4.3 Client Cache

The client cache is content-addressed:

```text
cache/
  ab/cd/<hash>.chunk
  19/83/<hash>.chunk
  manifests/
  temp/
```

A chunk can be reused by multiple files, versions, and assets.

### 4.4 Transport

The server sends only missing chunks. The client may compute the missing set locally by comparing the target manifest to its cache, or the server may maintain a session have-set.

Preferred scalable mode:

```text
1. Client downloads signed manifest.
2. Client verifies signature.
3. Client computes missing = manifest.chunks - cache.chunks.
4. Client requests missing hashes.
5. Server returns a zstd-compressed batch of chunks.
6. Client verifies each chunk and stores it directly to disk.
7. Client reconstructs the final file by streaming chunks from cache.
```

### 4.5 Constant-Memory Reconstruction

The optimized client does not build the target file in memory. It writes to a temporary `.part` file chunk by chunk, computes the final SHA-256 as bytes are written, and atomically renames the `.part` file only after verification succeeds.

This prevents corrupted partial downloads from appearing as valid content packs.

---

## 5. Security Model

CAVS separates security into integrity, authenticity, confidentiality, and rollback protection.

| Property | Mechanism |
|---|---|
| Chunk integrity | hash per chunk |
| File integrity | final SHA-256 in manifest |
| Manifest authenticity | Ed25519/ECDSA/RSA-PSS signature |
| Transport safety | HTTPS/TLS |
| Corrupt partial files | `.part` then verify then atomic rename |
| Rollback protection | monotonically increasing build number / anti-rollback state |
| Private distribution | optional client-side encryption |

The recommended baseline:

```text
Chunk hash: BLAKE3 for speed + SHA-256 for compliance where needed
Final file hash: SHA-256
Manifest signature: Ed25519 for default; ECDSA P-256 for enterprise/FIPS-sensitive deployments
Transport: HTTPS
```

---

## 6. Experimental Methodology

The benchmark corpus is real Godot projects exported as PCK files from two historical references per repository — public, reproducible payloads representative of large mixed-content binaries. The layer itself is payload-agnostic. The tests measured HTTP egress in actual client/server sessions.

### 6.1 Baselines

- zstd full-file download;
- zip full-file download;
- rsync wire transfer;
- rdiff/librsync;
- xdelta3 -9;
- bsdiff;
- CAVS wire update.

### 6.2 Metrics

- update payload in MiB;
- storage overhead;
- pack time;
- client wall time;
- client CPU;
- peak RSS;
- cache reuse count;
- integrity result;
- runtime mount success in Godot.

---

## 7. Results

### 7.1 Benchmark corpus: real Godot projects

| Project | Versions | PCK v2 | PCK.zst baseline update | CAVS update | Delta |
|---|---|---:|---:|---:|---:|
| marble | 1.6.0 -> 1.6.1 | 9.59 MiB | 6.55 MiB | 0.19 MiB | -97.1% |
| gdquest | HEAD~10 -> HEAD | 61.09 MiB | 27.61 MiB | 13.27 MiB | -51.9% |
| tps | tag 4.5 -> master | 569.15 MiB | 247.60 MiB | 4.97 MiB | -98.0% |

All tested cases were byte-identical after reconstruction and were mountable by the Godot plugin.

### 7.2 Pairwise Delta Comparison

| Project | zstd full | zip full | rsync wire | rdiff | xdelta3 -9 | bsdiff | CAVS wire |
|---|---:|---:|---:|---:|---:|---:|---:|
| marble | 6.55 | 6.34 | 0.02 | 0.01 | 0.00 | 0.00 | 0.19 |
| gdquest | 27.61 | 27.42 | 54.86 | 7.06 | 3.78 | 3.82 | 13.27 |
| tps | 247.60 | 247.51 | 462.95 | 0.70 | 0.03 | 0.03 | 4.97 |

Pairwise delta tools win the pure byte-count sprint for a single v1-to-v2 jump. CAVS competes on operational scalability: one package per release, cross-version reuse, cache persistence, re-download elimination, resumability, and no need for every version-pair patch.

### 7.3 Delta Generation Cost

| Project | xdelta3 make | bsdiff make | bsdiff RSS | CAVS pack v2 | CAVS pack RSS |
|---|---:|---:|---:|---:|---:|
| marble | 0.7 s | 1.4 s | 186 MiB | 0.9 s | 15 MiB |
| gdquest | 1.0 s | 15.2 s | 1171 MiB | 4.9 s | 67 MiB |
| tps | 4.0 s | 137.6 s | 9107 MiB | 40.0 s | 576 MiB |

### 7.4 Chunk Size Sweep

| Project | Avg chunk | Unique chunks | Update MiB | .cavs MiB |
|---|---:|---:|---:|---:|
| marble | 64 KiB | 129 | 0.14 | 6.91 |
| marble | 256 KiB | 32 | 0.19 | 6.69 |
| marble | 1024 KiB | 7 | 0.21 | 6.57 |
| gdquest | 64 KiB | 675 | 8.70 | 28.86 |
| gdquest | 256 KiB | 160 | 13.27 | 28.17 |
| gdquest | 1024 KiB | 42 | 20.13 | 27.89 |
| tps | 64 KiB | 5747 | 1.64 | 255.29 |
| tps | 256 KiB | 1424 | 4.97 | 252.01 |
| tps | 1024 KiB | 353 | 16.08 | 250.32 |

Recommendation for large-binary distribution: **CDC avg 64 KiB, min 16 KiB, max 256 KiB, zstd level 3**.

### 7.5 Constant-Memory Client Optimization

| Metric | Before | After debug | After release |
|---|---:|---:|---:|
| Peak client RSS, update | 1124 MiB | 10 MiB | 7 MiB |
| Peak client RSS, cold install | - | 10 MiB | 7 MiB |
| Update payload | 4.97 MiB | 1.64 MiB | 1.64 MiB |
| CPU update | 7.0 s | 28.9 s | 2.0 s |
| Pack one release, 569 MB | 40 s | - | 3.5 s |

The optimized client streams the CVSP batch, writes verified chunks directly into the disk cache, reconstructs to a temporary file, verifies the final SHA-256, and performs an atomic rename.

---

## 8. Discussion

CAVS is strongest when:

- clients already have a prior version;
- many versions exist simultaneously;
- updates are frequent;
- content is large and asset-based;
- downloads must resume cleanly;
- a launcher, engine plugin, or CI/CD pipeline can control the update flow.

CAVS is weaker when:

- only a single v1-to-v2 pair matters;
- an optimal pairwise delta patch is acceptable;
- the content is already compressed and unrelated;
- there is no persistent client cache;
- the platform already provides an equivalent chunk-store update model.

The correct positioning is:

```text
CAVS is not a universal compression algorithm.
CAVS is a content-addressable verified update and delivery layer.
```

---

## 9. Product Implications

The first product candidate is **CAVS for Godot**, because the PCK workflow is simple, demonstrable, and the plugin already mounts reconstructed packs at runtime.

The second capability is **SteamPipe-style analysis inside the CAVS CLI** (shipped in v0.9.0 as `cavs analyze steampipe`, `cavs bench steampipe-style`, `cavs publish-preview` and related commands). It does not replace SteamPipe and is deliberately not a separate product: it diagnoses pack-file update bloat under a public fixed-1MiB model before a developer publishes a build.

The third candidate is **CAVS Delivery SDK** for launchers, Unity, Unreal, and enterprise asset distribution.

---

## 10. Conclusion

CAVS-1 demonstrates that a content-addressable chunk store, signed manifests, missing-set transport, zstd batch compression, and constant-memory verified reconstruction can dramatically reduce large-file update payloads versus full-package distribution while avoiding the operational complexity of per-version-pair deltas. The results justify further work on engine integrations, SteamPipe analysis tooling, formal security review, and patent filing.

---

## References

1. Valve Steamworks Documentation. "Uploading to Steam" and SteamPipe content system documentation.
2. USPTO. "Provisional Application for Patent" and Patent Center filing materials.
3. Tridgell, A. and Mackerras, P. rsync algorithm and rsync implementation history.
4. Benet, J. "IPFS - Content Addressed, Versioned, P2P File System," arXiv:1407.3561.
5. Git documentation. Git object model and packfiles.
6. FastCDC and content-defined chunking literature.
7. CAVS benchmark artifacts generated during this research: `RESULTADOS.md`, `RESULTADOS_JUEGOS_REALES.md`, `RESULTADOS_COMPARATIVAS(1).md`.
