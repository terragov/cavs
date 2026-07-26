# Serverless / CDN-only delivery (v1.4.0)

CAVS can deliver updates with **no running `cavs-server`**. You export a
release once as an immutable, self-describing static tree and upload it to any
object store or static host; clients plan their own fetch and download only
the chunks they lack, over concurrent HTTP Range requests, verified end to
end. This is the cheapest way to ship: `−99%`-class incremental updates with
zero delivery infrastructure to run.

## The static tree

`cavs store export --static-plans` writes a deterministic tree from a
packfile-layout store:

```text
dist/
  chunks/packs/<ab>/<id>.cavspack        immutable, content-addressed
  chunks/indexes/<ab>/<id>.cavsindex     immutable
  assets/<name>/record.json              release ledger
  assets/<name>/manifest.json            reconstruction structure (v1.4.0)
  assets/<name>/chunk-map.json           hash → pack + absolute byte range
```

Two files make the tree self-sufficient for a serverless client:

- **`manifest.json`** — the runtime manifest (tracks/files and their ordered
  chunk hashes): *what to reconstruct and in what order*.
- **`chunk-map.json`** — for every chunk: its pack file, `pack_offset_abs`
  (the absolute byte offset for a Range request), `len_stored`, `len_raw` and
  `flags` (zstd or not): *where each chunk is and how to decode it*.

Both are plain JSON; nothing about the packfile format leaks to the client.

## Publish

```sh
# 1. Package the build and ingest into a packfile-layout store
cavs pack-dir ./Build --profile auto -o build.cavs
cavs store ./store add app build.cavs --storage packfiles

# 2. (later versions) ingest each new build; shared chunks are stored once
cavs store ./store add game_v2 build_v2.cavs

# 3. Export the static tree and upload it
cavs store ./store export --out ./dist --static-plans
aws s3 sync ./dist s3://your-bucket/         # or rclone / gh-pages / nginx docroot
```

The exported tree is immutable and content-addressed, so it is ideal behind a
CDN: every object can be cached forever (`Cache-Control: immutable`).

## Install and update — no server

```sh
# Cold install (downloads the whole build once)
cavs-client fetch-static https://cdn.example.com/dist app \
  -o ./install --cache ./cache --connections 8

# Later: update to the new version reusing the cache — only changed chunks
cavs-client fetch-static https://cdn.example.com/dist app \
  -o ./install --cache ./cache --connections 8
```

`base` may be an `http(s)://` URL (Range GETs) or a local directory (slice
reads, for offline mirrors). The client:

1. fetches `manifest.json` + `chunk-map.json`,
2. computes the missing set against the persistent content-addressable cache,
3. downloads only the missing chunks — concurrently, `--connections` at a time
   — Range-reading each from its pack, decompressing if flagged, and verifying
   its BLAKE3 before caching it,
4. reconstructs the output files from the cache (`.part` → verify → atomic
   rename; a torn file is never promoted).

Signed releases are enforced with `--pubkey <hex|file>` exactly as with the
online client.

## Programmatic / embedded

The same engine is a library (`cavs-fetch`) and an SDK operation
(`fetchStatic` in Go/Kotlin/Node) and is what the Unity and Unreal plugins
call to self-update in-process — see
[docs/EMBEDDABLE_FETCH.md](EMBEDDABLE_FETCH.md).

## When to use which path

| | Static / serverless (`fetch-static`) | Online (`fetch`, session/batch) |
|---|---|---|
| Infrastructure | none — any static host | a running `cavs-server` |
| Best transport | CDN edge cache per chunk | single smart origin |
| Server-side pack read-coalescing | n/a (client Range-reads) | yes (170× fewer reads) |
| Parallel downloads | always | opt-in `--connections N` |

Both deliver the same incremental egress; they differ in where the work runs.
For a single packfile origin without a CDN, the online path's coalescing is
more read-efficient; behind a CDN, the serverless path is simpler and cheaper.
