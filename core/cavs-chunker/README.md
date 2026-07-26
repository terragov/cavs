# cavs-chunker

Chunking strategies: how content is split into chunks before hashing.

## What it does

- **Fixed** — fixed-size chunks aligned to the start (stable, CDN-friendly for
  already-packaged media segments).
- **FastCDC** — content-defined chunking that resists insertions and
  reordering: unchanged regions produce identical chunks even after a shift.
  `ChunkMode::Cdc` carries the FastCDC normalization level (`norm`): it
  changes boundary placement, so it is part of a profile's identity —
  `NORM_DEFAULT` (level 1) reproduces the pre-1.3 boundaries bit-for-bit,
  `NORM_TIGHT` (level 3) concentrates chunk sizes around the average and is
  what the small `fastcdc-16k`/`fastcdc-32k` profiles use.

Presets: `media_default()` (256 KiB fixed), `asset_default()` (FastCDC
16/64/256 KiB — the large-binary default), `screen_default()` (aggressive CDC).

## Use

```rust
use cavs_chunker::{split, ChunkMode};
for range in split(&data, ChunkMode::asset_default()) {
    let chunk = &data[range];
    // hash / store chunk
}
```
