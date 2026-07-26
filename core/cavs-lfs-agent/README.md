# cavs-lfs-agent

A [Git LFS](https://git-lfs.com) **standalone custom transfer agent** backed
by CAVS: large files are content-defined-chunked (FastCDC), zstd-compressed
and stored in a shared content-addressable store — so across versions of a
file **only the chunks that actually changed travel and get stored**. Vanilla
Git LFS re-uploads and re-stores every version whole; with CAVS a 3 MiB edit
to a 22 MiB asset costs ~3 MiB at the remote, not 22 MiB.

Downloads verify BLAKE3 per chunk plus the LFS oid (sha256) end to end, reuse
a local chunk cache across versions, and fetch missing chunks with concurrent
range requests — from a plain directory, or straight off a CDN/static host
over HTTP.

## Install

```sh
cargo install cavs-lfs-agent
# or grab a prebuilt binary from the GitHub releases
```

## Setup (per repository)

```sh
git lfs install --local
git lfs track '*.bin' '*.uasset' '*.png'   # whatever is large

git config lfs.standalonetransferagent cavs
git config lfs.customtransfer.cavs.path /path/to/cavs-lfs-agent
git config lfs.customtransfer.cavs.concurrent false
```

That's it — `git push` / `git pull` / `git clone` move LFS objects through
CAVS. The agent resolves the remote from the git remote URL; override it with
either:

```sh
git config lfs.customtransfer.cavs.args "--remote /srv/lfs-cavs"
# or
export CAVS_LFS_REMOTE=/srv/lfs-cavs
```

Cloning needs the config to exist *before* checkout:

```sh
git clone \
  -c 'filter.lfs.smudge=git-lfs smudge -- %f' \
  -c 'filter.lfs.clean=git-lfs clean -- %f' \
  -c 'filter.lfs.process=git-lfs filter-process' \
  -c filter.lfs.required=true \
  -c lfs.standalonetransferagent=cavs \
  -c lfs.customtransfer.cavs.path=cavs-lfs-agent \
  -c lfs.customtransfer.cavs.concurrent=false \
  /srv/assets.git
```

(or `GIT_LFS_SKIP_SMUDGE=1 git clone …`, configure, then `git lfs pull`).

## Remotes

| Remote | Downloads | Uploads |
|---|---|---|
| directory / `file://` (disk, NAS, synced folder) | ✅ | ✅ |
| `http(s)://` (CDN, object-storage website, `cavs serve`) | ✅ | ❌ read-only |

When the remote path is a **bare git repository**, the CAVS tree lives at
`<repo>.git/cavs/` automatically — push LFS objects to the same place you
push commits, zero extra configuration.

Layout at a directory remote:

```
<remote>/
  .store/                      shared content-addressable store (dedup)
  assets/<oid>/manifest.json   per-object reconstruction structure
  assets/<oid>/chunk-map.json  chunk → pack byte ranges
  chunks/packs/…               immutable content-addressed packfiles
```

`assets/` + `chunks/` are a **static CAVS export**: rsync/aws-sync them to a
CDN and point clients at the URL — clones then pull over HTTP with no server
(uploads keep going to the directory remote).

## Options

Pass via `lfs.customtransfer.cavs.args`:

| Flag | Default | |
|---|---|---|
| `--remote <path\|url>` | git remote URL | where objects live (`$CAVS_LFS_REMOTE`) |
| `--cache-dir <dir>` | `<git-dir>/lfs/cavs/cache` | chunk cache (`$CAVS_LFS_CACHE`) |
| `--profile <p>` | `auto` (= fastcdc-64k) | chunking: `fastcdc-16k/32k/64k/128k[-n3]`, `fixed-256k/512k/1m` |
| `--compression <c>` | `zstd-3` | `none` or `zstd-<1..22>` |
| `--no-bg4` | off | disable the per-chunk BG4 byte-grouping pretransform (numeric payloads: model weights, vertex buffers, audio) |
| `--connections <n>` | 8 | parallel download connections |
| `--pubkey <hex>` | — | require Ed25519-signed manifests on download |
| `--sign-key <file>` | — | sign uploads (64-hex secret key) |

Downloads honour `CAVS_FETCH_MAX_INFLIGHT_BYTES` (default 128 MiB): a
process-wide cap on range-request bytes in flight, so many parallel
downloads can't stack unbounded buffers.

## Limitations

- Uploads need a writable **directory** remote; syncing the tree to S3/R2 is
  an external step (rclone, `aws s3 sync`).
- Keep `lfs.customtransfer.cavs.concurrent false`: transfers within one agent
  are sequential (downloads still parallelize internally per object). A file
  lock serializes concurrent pushes from several machines to a shared
  filesystem remote — advisory locks can be unreliable on NFS.
- The directory remote stores the packed data twice (store + static export)
  in exchange for a CDN-syncable tree; exports skip unchanged packs.
- Publication is session-batched (Xet-style finalize): a push's objects
  become fetchable when the agent finalizes at terminate, not per object.
  Ingested packs aggregate across the whole push and the store ledger is
  committed once; a push killed before finalize publishes nothing, and the
  retry re-ingests and repairs.
