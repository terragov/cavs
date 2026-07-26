# The local app/depot/branch/build workspace (v0.9.0)

SteamPipe organizes content as apps that own depots, builds that
snapshot depots, and branches that point at builds. CAVS models the
same concepts as **local metadata only** — for testing, benchmarking
and organizing releases. No accounts, no uploads, no platform.

## Quick start

```sh
cavs workspace init ./cavs-workspace --app my-app

cavs depot add base
cavs depot add windows --platform windows
cavs depot add linux   --platform linux
cavs depot add lang-es --language es
cavs depot add hd-textures --optional

cavs branch add public
cavs branch add beta
cavs branch add nightly --private

cavs build create \
  --branch beta \
  --depot base=./Build/Base \
  --depot windows=./Build/Windows \
  --label build_1001
```

Every command takes `--workspace` (default `./cavs-workspace`) and
`--app` (default: the workspace's app).

## On disk

```text
cavs-workspace/
  cavs.toml                      # workspace: default app
  apps/my-app/
    app.toml                     # depots + branches (current builds)
    builds/build_1001/
      build.toml                 # label, created_at, depot sources
      base.index.json            # content chunk index per depot
    reports/
```

`build create` indexes each depot source with the CAVS chunker
(FastCDC 64 KiB, BLAKE3) and stores the chunk lists. Estimates —
sharing, promotion previews, install plans, the `/api/preview` endpoint
of `cavs serve` — come from these indices without re-reading the
sources. Metadata writes are atomic (temp file + rename).

## Depot semantics

| Flag | Effect on install plans |
|---|---|
| `--platform windows\|linux\|macos` | delivered only to matching `--platform` clients |
| `--language es` | delivered only to matching `--language` clients |
| `--optional` | delivered only when listed in `--owned` |

## Branches: promote, rollback, preview

```sh
cavs branch promote --branch public --build build_1002
cavs branch promote-preview --branch public --build build_1002   # per-depot cost first
cavs branch rollback --branch public --to build_1001
```

- Promotion is an atomic metadata change; every promotion appends to
  the branch history.
- **Rollback only re-points to a build the branch has served before**
  (`CAVS-E-BUILD-NOT-FOUND` otherwise).
- `promote-preview` estimates the per-depot update every client would
  download (new chunk bytes between the branch's current build and the
  candidate).

## Sharing analysis

```sh
cavs depot analyze-sharing --out sharing.md
```

Reports shared/unique bytes and reuse % for every depot pair of a
build, and suggests shared-depot splits when pairs overlap heavily.
Measured example (windows ↔ linux: 98.9% shared):
[results/v0.9.0/depot-sharing/](results/v0.9.0/depot-sharing/).

## Install plans

```sh
cavs install-plan \
  --branch public \
  --platform windows --language es \
  --owned base,dlc1,lang-es \
  --from build_1001 --to build_1002
```

Filters the target build's depots by platform/language/ownership, then
prices each depot: `no-op` (already up to date), `update` (new chunks
vs the installed build) or `install` — with **cross-depot reuse**:
chunks fetched for one depot (or already installed) are free for the
next. A trial-tier owner with the full product installed downloads 0 B.
Route suggestions per depot follow the size ratio (`.cavsplan` for
small updates, chunks/bootstrap otherwise). `--json` for machines.

## Serving a workspace

`cavs serve ./cavs-workspace --port 8990` exposes branches, builds,
depot files (with HTTP Range), chunks and update previews for local
development — see [LOCAL_CONTENT_SERVER.md](LOCAL_CONTENT_SERVER.md).

## Error taxonomy

`CAVS-E-WORKSPACE-CORRUPT`, `CAVS-E-DEPOT-NOT-FOUND`,
`CAVS-E-BRANCH-NOT-FOUND`, `CAVS-E-BUILD-NOT-FOUND`,
`CAVS-E-INSTALL-PLAN-INVALID` — stable prefixes, greppable in logs.
