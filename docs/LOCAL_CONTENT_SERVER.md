# The local development content server — `cavs serve` (v0.9.0)

SteamPipe documents a local content server for development. The CAVS
equivalent serves a **workspace** over plain HTTP for local testing —
client integration, the Godot plugin, benchmark scripts.

```sh
cavs serve ./cavs-workspace --app my-app --port 8990
```

> **Development only.** No auth, no TLS, binds 127.0.0.1, and prints a
> warning saying it is not production hardened. Production delivery is
> `cavs-server` (sessions, CVSP batches, TLS, CDN-shaped endpoints).

## Endpoints

```text
GET /                                                        banner + endpoint list
GET /api/apps/{app}/branches/{branch}/current                branch + current build metadata
GET /api/apps/{app}/builds/{build}                           build metadata
GET /api/apps/{app}/builds/{build}/depots/{depot}/index      content chunk index (JSON)
GET /api/apps/{app}/builds/{build}/depots/{depot}/files/{path}   raw file bytes (HTTP Range supported)
GET /api/apps/{app}/builds/{build}/depots/{depot}/chunks/{hash}  one chunk by BLAKE3 hash
GET /api/assets/{asset}/{file}                               published release files (--releases dir)
GET /api/preview?app=…&from=build_…&to=build_…               per-depot update estimate
```

- **Files** stream from the depot's recorded source directory with
  `Range: bytes=…` support (a client can fetch exactly the byte ranges
  a plan needs). Paths are validated against traversal
  (`CAVS-E-ANALYZE-PATH-TRAVERSAL`).
- **Chunks** are located through the depot index (cumulative offsets)
  and re-hashed before serving; if the source changed since indexing
  the server answers `410` with `CAVS-E-CHUNK-HASH-MISMATCH` instead of
  serving stale bytes.
- **Releases**: point `--releases` at a directory of published files
  (e.g. `cavs publish-dir` output) to serve manifests, `.cavs`
  containers, `.cavsplan`s, `.cavssig`s and bootstrap artifacts under
  `/api/assets/{asset}/{file}`, also with Range support.
- **Preview** mirrors `cavs branch promote-preview`: estimated new
  chunk bytes per depot between two builds.

## A typical dev loop

```sh
cavs workspace init ws --app my-app
cavs depot add base --workspace ws
cavs branch add beta --workspace ws
cavs build create --workspace ws --branch beta --depot base=./export
cavs serve ws --port 8990
# point the Godot plugin / a test client at http://127.0.0.1:8990
curl "http://127.0.0.1:8990/api/apps/my-app/branches/beta/current"
```

Because depot sources are referenced (not copied), re-exporting your
build and running `cavs build create` again is the whole publish step.
Record builds with **absolute** source paths if you plan to run the
server from another directory.
