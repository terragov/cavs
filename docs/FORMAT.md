# CAVS-1 — Binary format specification (v1.0)

CAVS-1 (*Content-Addressable Verified Streaming*, v1) is a content-addressable
container format for **large files** (application and machine builds, models,
datasets, images, packs, bundles, patches) — and, secondarily, video. It stores
builds, binary assets, or already-packaged segments (CMAF/fMP4) as
**deduplicated chunks identified by their BLAKE3-256 hash**, plus the tables
needed to reconstruct the original files byte-for-byte.
It is **not a pixel codec**: when it packages video, encoding stays in mature
codecs (H.264, HEVC, VP9, AV1…).

All integers are **little-endian**. All offsets are absolute from the start of
the file unless stated otherwise.

## Overall layout

```
+-----------------------------+  offset 0
| Superblock (64 bytes)       |
+-----------------------------+  offset 64
| DATA section                |  chunk payloads, streamed while packing
+-----------------------------+
| TRACKS section              |
| DICT section                |
| CHUNKS section              |
| SEGMENTS section            |
| META section                |
| INTEGRITY section           |
+-----------------------------+
| Section directory           |  pointed to by the superblock
+-----------------------------+
```

## Superblock (64 bytes, offset 0)

| Offset | Size | Field | Description |
|---:|---:|---|---|
| 0 | 4 | `magic` | `"CAVS"` |
| 4 | 2 | `version_major` | `1`. An unknown major invalidates forward reads |
| 6 | 2 | `version_minor` | `0`. A higher minor is backward-compatible |
| 8 | 4 | `feature_flags` | extension bitmap (0 in v1.0) |
| 12 | 1 | `hash_algo` | `1` = BLAKE3-256 |
| 13 | 1 | `compression_algo` | file default: `0` = none, `1` = zstd |
| 14 | 2 | reserved | 0 |
| 16 | 16 | `asset_uuid` | asset UUID |
| 32 | 4 | `timescale` | root timescale (ticks/second; 1000 = ms) |
| 36 | 4 | `section_count` | number of section-directory entries |
| 40 | 8 | `section_dir_offset` | absolute offset of the section directory |
| 48 | 8 | `file_size` | total file size |
| 56 | 8 | reserved | 0 |

Since 0.5.0 the reader validates `hash_algo`, `compression_algo` and
`file_size` (values a correct writer never produces are rejected). The
remaining superblock fields — `asset_uuid`, `timescale`, `feature_flags`,
`version_minor`, reserved bytes — are deliberately **unauthenticated
metadata**: content integrity is carried entirely by the section hashes,
the per-chunk hashes and the Merkle root (and pinned by the content
signature when present), never by the superblock. A full single-byte-flip
sweep in the `cavs-format` tests verifies that no flip outside those
metadata fields survives verification.

## Section directory

`section_count` consecutive entries of **52 bytes**:

| Size | Field | Description |
|---:|---|---|
| 4 | `section_type` | see table |
| 8 | `offset` | absolute offset of the section |
| 8 | `length` | length in bytes |
| 32 | `hash` | BLAKE3-256 of the section's raw bytes |

Types: `1` TRACKS, `2` DICT, `3` CHUNKS, `4` SEGMENTS, `5` DATA, `6` INTEGRITY,
`7` META. Readers must ignore unknown types (extensibility). All sections above
are mandatory in v1.0.

## Encoding conventions

- `str16`: u16 length + UTF-8 bytes.
- `bytes32`: u32 length + bytes.
- Chunks are referenced by their **u32 index** in the CHUNKS table (not by
  hash), keeping directories compact; the hash is available via the table.

## CHUNKS section

```
u32 count
count × {
  [32] hash        // BLAKE3-256 of the UNCOMPRESSED payload (identity)
  u64  data_offset // relative to the start of the DATA section
  u32  len_raw     // uncompressed length
  u32  len_stored  // stored length (== len_raw if not compressed)
  u32  flags       // bit0 = payload stored with zstd
}
```

Identity and integrity are separate: the identity (`hash`) is stable and
independent of storage compression. Deduplication is by identity — the same
hash appears **once** in the table and in DATA.

## DATA section

Concatenation of the stored (possibly compressed) payloads of the unique
chunks, in insertion order. No internal headers: boundaries come from the
CHUNKS table.

## TRACKS section

```
u32 count
count × {
  u32   track_id
  u8    kind          // 0 video, 1 audio, 2 subtitle, 3 data/asset
  u8    flags
  str16 codec         // e.g. "h264+aac", "m3u8", "raw"
  str16 name          // logical name (file stem, relative path…)
  u32   timescale
  u32   init_chunk_count
  init_chunk_count × u32   // init-segment chunks (CMAF init), in order
}
```

## DICT section (global dictionary)

```
u32 count
count × u32   // indices of privileged chunks (bootstrap, init segments,
              // shared assets) — candidates for prefetch/pinning
```

## SEGMENTS section

```
u32 count
count × {
  u64 segment_id
  u32 track_id
  u64 pts_start     // in root-timescale units
  u32 duration
  u32 flags         // bit0 = random-access point (keyframe bundle)
  u32 chunk_count
  chunk_count × u32 // reconstruction = ordered concatenation of the chunks
}
```

## META section

```
u32 count
count × { str16 key; bytes32 value (UTF-8) }
```

Packers emit per-file `sha256:<name>` entries so thin clients without BLAKE3
(for example the Godot GDScript runtime) can verify reconstruction with their
built-in hasher. A signed asset also carries `sig.ed25519` and `sig.pubkey`.

Since 0.1.2 the packer may also record (all additive — older readers ignore
unknown keys):

- `profile:<name>` / `payload_kind:<name>` — the chunk profile chosen for a
  track and the classified payload kind, for reproducibility and diagnostics.
- `bootstrap.name`, `bootstrap.size`, `bootstrap.blake3` — metadata of the
  **bootstrap sidecar**: a `<output>.cavs.bootstrap.zst` file written next to
  the container holding the whole (single-input) asset zstd-compressed. The
  sidecar is *outside* the container so chunks are never stored twice; its
  BLAKE3 recorded here is what binds it to the (optionally signed) container.
  A server only offers a sidecar that verifies against these entries, and a
  client verifies the artifact again end to end (BLAKE3 of the wire bytes +
  per-file SHA-256) before installing and seeding its cache from it.

## INTEGRITY section

```
[32] merkle_root   // binary Merkle root over the CHUNKS table hashes, in table
                   // order; nodes = blake3(left || right); an odd node is
                   // promoted unchanged; the empty list = blake3("")
u64  chunk_count
u64  total_raw     // unique uncompressed bytes
u64  total_stored  // unique stored bytes
```

## Verification model (three layers)

1. **Per chunk**: on read, `blake3(decompressed payload) == hash` and
   `len == len_raw`.
2. **Per section**: BLAKE3 of each section against the directory (tables are
   verified on open; DATA is verified by `verify`).
3. **Global**: Merkle root of the chunk table against INTEGRITY (detects table
   tampering and enables Bao-style incremental verification later).

The reader validates every offset and length against the real file size and
bounds all allocations (`MAX_CHUNK_RAW`, table capacities ≤ section bytes)
before reserving memory: a malformed or adversarial `.cavs` yields an error,
never a panic or OOM. An interoperability test vector (Merkle root over fixed
inputs) is pinned in the `cavs-hash` tests.

## Content signature (Ed25519, optional)

The signed message is `"CAVS1-SIG-V1" || merkle_root || chunk_count`. It covers
every content byte; the table/segment structure is protected by the per-section
hashes and by TLS in transit. The signature and signer public key are embedded
in META (`sig.ed25519`, `sig.pubkey`) and exposed in the manifest so clients can
enforce a trusted key.

## Global store (content-addressable at rest)

A `.cavs` file is portable and self-contained. To serve many versions/titles
without duplicating bytes, `cavs store <dir> add` ingests `.cavs` files into a
global CAS: each unique chunk (by BLAKE3) is stored **once**, with an
`index.json` ledger holding a per-chunk reference count, and a per-asset
record in `assets/<name>.json`. `rm` decrements reference counts; `gc`
reclaims zero-ref chunks after a grace period. `cavs-server --store <dir>`
serves directly from the store, so deduplication savings apply to **origin
storage**, not just client egress.

Two physical layouts, fixed when the store is created:

- **`loose`** (default): one file per chunk under `chunks/<ab>/<hex>` — the
  pre-0.4.0 behavior, still fully supported.
- **`packfiles`** (`add --storage packfiles`, since 0.4.0): chunks appended
  into immutable `.cavspack` files, read by range. The ledger records each
  chunk's pack and offset; GC deletes a pack once no live chunk references
  it (partial compaction is deliberately out of scope for 0.4.0).

## Packfiles — `.cavspack` and `.cavsindex` (since 0.4.0)

Object-per-chunk storage is operationally expensive at scale (a 570 MB build
is ~6,000 small files). Packfiles keep the same content-addressed identity
model with a production-friendly physical shape. Chunks are written in
reconstruction order, so update fetches touch mostly-contiguous ranges; the
server coalesces chunk reads within a 64 KiB gap into single physical reads
(capped at 8 MiB), measured at 65–170× fewer reads on real builds with 1.000
read amplification.

### `.cavspack` layout

```
Header (16 bytes):
  magic          8 bytes  "CAVSPK1\0"
  version_major  u16 LE   1
  version_minor  u16 LE   0
  flags          u32 LE   reserved (0)
Chunk data region:
  concatenated stored chunk bytes (no per-chunk framing; boundaries live
  in the index)
Footer (40 bytes):
  magic          8 bytes  "CAVSPEND"
  pack_hash      [32]     BLAKE3 of every byte before the footer
```

The **pack id** is the BLAKE3 of the entire file; the filename is derived
from it (`packs/<ab>/<id>.cavspack`), so packs are immutable and directly
CDN-cacheable. A pack is closed once its data region reaches the preferred
size (128 MiB; small assets produce a single pack).

### `.cavsindex` sidecar

One per pack, written at close — the chunk table needed to read the pack
without the store ledger (recovery, `store export`):

```
  magic          8 bytes  "CAVSIDX1"
  pack_id        [32]
  entry_count    u32 LE
  entry_count × {
    hash        [32]
    offset      u64 LE   // into the pack's data region
    stored_len  u32 LE
    raw_len     u32 LE
    flags       u32 LE
  }
  body_hash      [32]     BLAKE3 of every byte before this field
```

### Object-store export

`cavs store <dir> export --out <dist>` writes a deterministic immutable tree
ready to upload to S3/R2/a static host behind a CDN:

```
dist/
  chunks/packs/<ab>/<id>.cavspack     Cache-Control: public, max-age=31536000, immutable
  chunks/indexes/<ab>/<id>.cavsindex  ETag: "blake3-<id>"
  assets/<name>/record.json           Cache-Control: no-cache (mutable per release)
```

## Reconstruction to playable media

For a video track packaged from CMAF/HLS:

- `init.mp4` = concatenation of the track's `init_chunks`.
- `seg_NNNNN.m4s` = each segment's bytes, ordered by `pts_start`.
- The original HLS playlist is kept as a companion data track
  (`name = "<stem>/media.m3u8"`).
- A valid progressive MP4 = `init.mp4` + all `.m4s` concatenated (fMP4). These
  are the same bytes a browser would append to an MSE `SourceBuffer`.

## Manifest wire format v2 — `CAVSMF2` (since 0.3.0)

The runtime **manifest** (what a server announces to clients so they can plan
a fetch) is separate from the `.cavs` container. Two wire formats carry the
same runtime model:

- **JSON v1** — the original human-readable manifest. Still the default
  response of `GET /api/assets/{asset}/manifest`, the debug export
  (`cavs manifest export`) and the compatibility path for old clients.
- **Binary v2** — a compact sectioned encoding, served when the client asks
  for it (`Accept: application/vnd.cavs.manifest-v2` or `?format=binary-v2`).
  Implemented in the `cavs-manifest` crate; ~75–77% smaller than JSON v1 on
  real 64 KiB-chunked builds, with parse time at parity.

Readers detect the format from the bytes themselves (`CAVSMF2\0` magic vs
JSON), so no out-of-band hint is needed.

### Envelope

Integers are unsigned **LEB128 varints** unless sized; scalars little-endian.
Varint decoding is strict: truncated input, more than 10 bytes, bits beyond
u64 and overlong encodings (a redundant trailing zero continuation byte) are
rejected, so every value has exactly one valid wire form.

```
Header:
  magic          8 bytes   "CAVSMF2\0"
  version_major  u16       2 (an unknown major invalidates the read)
  version_minor  u16       0
  flags          u32       reserved (0)
  hash_alg       u8        1 = BLAKE3-256
  section_count  varuint   max 64

Section table (section_count entries):
  kind           varuint
  compression    u8        0 = none, 1 = zstd
  offset         varuint   into the data region (after the table)
  stored_len     varuint
  raw_len        varuint
  hash           [32]      BLAKE3-256 of the RAW (uncompressed) section

Data region: the sections' stored bytes, in table order.
```

Section kinds: `1` AssetInfo, `2` ChunkPlan, `3` ChunkDictionary,
`4` ChunkLocations (optional, since 0.4.0). Unknown kinds are skipped
(forward compatibility); kinds 1–3 are mandatory and no kind may repeat.
Sections whose raw encoding is ≥ 32 KiB are zstd-compressed (level 3) when
that actually shrinks them.

### Sections

Strings are `varuint length + UTF-8` (≤ 64 KiB). `Option` is a 0/1 byte tag.

```
AssetInfo (1):
  asset          str
  asset_uuid     str
  merkle_root    str            // hex, may be empty
  signature      Option<str>    // hex Ed25519, if signed
  signer_pubkey  Option<str>
  meta_count     varuint
  meta_count × { key str; value str }

ChunkDictionary (3):
  count              varuint
  chunk_table_count  varuint    // first N entries = the container's chunk
                                // table, in Merkle leaf order
  count × {
    hash  [32]                  // raw BLAKE3-256 (not hex)
    len   varuint               // raw chunk length
  }

ChunkPlan (2):
  track_count varuint
  track_count × { track_id varuint; kind str; codec str; name str;
                  timescale varuint; n varuint; n × dict_index varuint }
  segment_count varuint
  segment_count × { segment_id varuint; track_id varuint; pts_start varuint;
                    duration varuint; random_access u8;
                    n varuint; n × dict_index varuint }
  dict_pin_count varuint
  dict_pin_count × dict_index varuint
```

The encoding win over JSON v1: each unique chunk hash is stored **once**, as
32 raw bytes in the dictionary, and every chunk reference in the plan is a
1–2 byte varint index — versus a repeated 64-char hex string plus field names
per reference in JSON.

```
ChunkLocations (4, optional — packfile hints, since 0.4.0):
  pack_count varuint
  pack_count × [32]              // content-addressed pack ids
  entry_count varuint
  entry_count × {
    dict_index varuint           // into ChunkDictionary
    pack_ord   varuint           // into the pack table above
    offset     varuint           // into the pack's data region
    stored_len varuint
  }
```

Emitted by servers whose asset lives in a packfile store. **Advisory only**:
a consumer must verify chunk bytes by BLAKE3 regardless of where a hint
pointed, and fall back to an index lookup when a hint is missing or stale.
0.3.0 readers skip the section as an unknown kind.

### Decoder hardening

The decoder never trusts a length before validating it: input capped at
256 MiB, section bounds checked against the data region, `raw_len` bounded
both absolutely and relative to `stored_len` (≤ 100×, the zstd-bomb guard),
counts bounded by the bytes that could actually encode them, dictionary
indexes bounded by the dictionary, and every section must be consumed
exactly. Section hashes make any payload corruption a clean
`SectionHashMismatch` error. This is exercised by truncation sweeps and a
full byte-flip corruption sweep in the `cavs-manifest` tests, by the
`cavs test corrupt` mutation matrix, and by the libFuzzer targets under
`fuzz/` (since 0.5.0). The same discipline applies to the CVSP batch
decoders: item counts are never pre-allocated beyond what the buffer could
encode, and inline chunk lengths are validated against a 256 MiB wire
ceiling (`MAX_WIRE_CHUNK`) before any allocation.

## Planned extensions (v1.x, via `feature_flags` + new sections)

- Sub-chunk delta (`PatchBytes`, delta against a base chunk). Under research:
  it breaks the invariant that each chunk is self-describing by its own hash,
  and benchmarks show 64 KiB chunks already land within an order of magnitude
  of an ideal delta without paying that cost — deliberately kept out of v1.
- Perceptual / similarity-aware IDs for near-identical content.
- Per-chunk AEAD encryption and a CENC path for EME.
- Session have-set summarized with a Bloom filter (already implemented in
  CVSP: clients send `have_bloom` for large caches; false positives are
  repaired by fetching the chunk directly by hash).
