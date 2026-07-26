# cavs-manifest

Manifest wire formats for CAVS: the compact binary v2 codec and the JSON v1
compatibility reader.

## What it does

- **`read_manifest(bytes)`** — detects the wire format from the bytes
  themselves (`CAVSMF2\0` magic vs JSON) and decodes either into the same
  runtime `cavs_proto::Manifest`, so servers and clients never branch on
  formats downstream.
- **Binary manifest v2 (`CAVSMF2`)** — a sectioned envelope (AssetInfo,
  ChunkPlan, ChunkDictionary) with per-section BLAKE3 integrity. Unique chunk
  hashes are stored once as raw 32-byte BLAKE3 in a dictionary; every chunk
  reference in the plan is a varint dictionary index instead of a repeated
  64-char hex string. Sections ≥ 32 KiB are zstd-compressed. Measured on real
  real builds: **~75–77% smaller** than the JSON v1 equivalent, parse time at
  parity.
- **Strict varint codec** — unsigned LEB128 with hard limits; truncated,
  overlong and out-of-range encodings are rejected, so every value has exactly
  one valid wire form.
- **Hardened decoding** — input size, section count/bounds, decompression
  ratio (zstd-bomb guard), string lengths and dictionary indexes are all
  validated before any allocation. Malformed or hostile manifests fail with a
  structured error, never a panic — exercised by truncation and byte-flip
  sweeps in the tests.
- **`manifest_from_reader`** — builds the runtime manifest of a packed `.cavs`
  file, as used by `cavs manifest export` / `cavs manifest bench`.

## Use

```rust
use cavs_manifest::{read_manifest, encode_manifest_v2, ManifestFormat};

// Decode either wire format (e.g. an HTTP manifest response body).
let loaded = read_manifest(&bytes)?;
assert!(matches!(loaded.format, ManifestFormat::BinaryV2 | ManifestFormat::JsonV1));
let manifest = loaded.manifest; // cavs_proto::Manifest, format-independent

// Encode the compact format (a server serving binary v2).
let wire = encode_manifest_v2(&manifest)?;
```

The byte-level specification lives in
[`docs/FORMAT.md`](https://github.com/orelvis15/cavs-oss/blob/main/docs/FORMAT.md).
