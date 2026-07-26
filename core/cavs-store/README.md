# cavs-store

Content-addressable storage for CAVS.

## What it does

- `CasIndex` — an in-memory hash→index map with reference counts, used while
  packing to deduplicate chunks.
- `GlobalStore` — an on-disk global content-addressable store: every unique
  chunk is stored once across all assets and versions, with an `index.json`
  reference-count ledger and per-asset records. Supports `publish` /
  `unpublish` and garbage collection of zero-reference chunks after a grace
  period.
- Two physical **layouts**, fixed at store creation:
  - `loose` — one file per chunk (`chunks/<ab>/<hex>`), the pre-0.4.0
    behavior, still fully supported.
  - `packfiles` (0.4.0) — chunks appended in reconstruction order into a few
    large immutable `.cavspack` files (content-addressed: the filename is the
    BLAKE3 of the file), each with a verifiable `.cavsindex` sidecar. Reads
    go by range, and `read_chunks_stored_batch` **coalesces** nearby chunks
    of one pack into single physical reads. GC deletes a pack once no live
    chunk references it. `export_object_store` emits a deterministic
    immutable tree ready for S3/R2/CDN.

This is what turns per-file egress dedup into real **at-rest storage dedup** on
the origin — measured on a 570 MB real build: 5,775 chunk objects become 6
files, an update session's 5,775 chunk reads become 34 physical reads (170×
fewer) with 1.000 read amplification. Driven by `cavs store …` and
`cavs-server --store`.

## Use

```rust
use cavs_store::{GlobalStore, StoreLayout};
let mut store = GlobalStore::open_with_layout(
    "./store".as_ref(),
    Some(StoreLayout::Packfiles), // applies when the store is created
)?;
store.put_chunk(&hash, &stored_bytes, flags, len_raw)?;
store.publish_asset(&record)?;   // increments refcounts, closes the pack
let (batch, stats) = store.read_chunks_stored_batch(&hashes)?; // coalesced
let (removed, bytes) = store.gc(0)?;   // reclaim unreferenced chunks/packs
```
