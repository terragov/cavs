# Try CAVS on your own build

Run CAVS locally against two builds, compare update routes, inspect why
updates are large, and generate a release certification report. No CDN,
no account, no hosted anything — everything below runs on your machine.

## Who this guide is for

Use this guide if you have:

- two versions of any large build or artifact;
- a packed container (archive, image, model checkpoint, Godot PCK, …);
- a folder-based release or export;
- add-on, DLC or language packs;
- a custom installer or launcher workflow;
- large updates you want to understand before release.

## Install CAVS

Download the latest release from GitHub, or build from source:

```bash
cargo build --release        # → target/release/{cavs,cavs-server,cavs-client}
```

Check the version:

```bash
cavs --version               # cavs 1.0.0
```

Optional tools for external comparisons (CAVS skips them when missing):

```bash
butler --version
xdelta3 -V
bsdiff
```

## Quick start: compare two builds

```bash
cavs publish-preview ./Build_v2 \
  --previous ./Build_v1 \
  --routes all \
  --out preview
```

Open `preview/preview.md` and look for: the recommended route, the
estimated download size, apply time, disk I/O, and pack layout
warnings. The comparison covers full download, the SteamPipe-style
estimate, CAVS chunk/hybrid, the CAVS `.cavsplan`, and butler /
bsdiff / xdelta3 when installed.

## Certify an update before release

```bash
cavs certify \
  --old ./Build_v1 \
  --new ./Build_v2 \
  --profile release \
  --out ./certification
```

Open `certification/summary.md`. A successful report starts with
`Result: PASS`. CAVS checks byte-identical reconstruction, route
selection per client state, update size, disk I/O, SteamPipe-style
behavior, pack layout health, regressions when a baseline is provided,
and optional butler/xdelta/bsdiff comparisons.
Full reference: [CERTIFICATION.md](CERTIFICATION.md).

## Use case: Godot PCK update

```bash
cavs certify godot \
  --old-pck old.pck \
  --new-pck new.pck \
  --out ./certification/godot
```

CAVS verifies the reconstructed PCK is byte-identical and suitable for
the runtime flow — the plugin stays simple
([GODOT_PLUGIN.md](GODOT_PLUGIN.md)):

```gdscript
var cavs := CavsClient.new("http://127.0.0.1:8990")
var result := cavs.fetch("main_pack")
if result.ok:
    ProjectSettings.load_resource_pack(result.files[0])
```

## Use case: understand why an update is large

```bash
cavs analyze steampipe ./Build_v1 ./Build_v2 --out steampipe-analysis.md
cavs analyze-packs ./Build_v1 ./Build_v2 --out pack-analysis.md
```

The reports identify scattered pack-file churn, asset shuffling,
distributed TOC/offset churn, compressed blobs, timestamp/build-id
churn, oversized packs and new content inserted into old packs — and
what to do about each (split packs, group volatile assets, per-asset
compression, …).

## Use case: empty cache but previous install exists

Common when migrating an existing install base to CAVS: the client has
`build_v1.bin` but no CAVS cache yet. Hybrid reconstruction reuses
verified ranges straight from the installed artifact:

```bash
cavs-client fetch http://127.0.0.1:8990 build_v2 \
  --previous-artifact ./build_v1.bin \
  --cache ./cache \
  -o build_v2.bin
```

## Use case: folder-based builds

```bash
cavs pack-dir ./Build_v1 --profile auto -o build_v1.cavs
cavs signature export --raw ./Build_v1 -o build_v1.cavssig
cavs preview ./Build_v2 --against build_v1.cavssig

cavs diff-plan ./Build_v1 ./Build_v2 --out update.cavsplan
cavs apply --old ./Build_v1 --plan update.cavsplan --out ./Build_v2_out --verify

cavs signature export --raw ./Build_v2 -o build_v2.cavssig
cavs verify-install ./Build_v2_out --signature build_v2.cavssig
```

## Use case: depots, DLC and language packs

```bash
cavs workspace init ./cavs-workspace --app my-app
cavs depot add base    --workspace ./cavs-workspace
cavs depot add windows --platform windows --workspace ./cavs-workspace
cavs depot add lang-es --language es --optional --workspace ./cavs-workspace
cavs depot add dlc1    --optional --workspace ./cavs-workspace
cavs branch add beta   --workspace ./cavs-workspace

cavs build create --workspace ./cavs-workspace --branch beta \
  --depot base=./Build/Base --depot windows=./Build/Windows \
  --depot lang-es=./Build/Lang/es --label build_1001

cavs install-plan --workspace ./cavs-workspace --app my-app \
  --branch beta --platform windows --language es \
  --owned base,lang-es --from build_1001 --to build_1002
```

CAVS reports per-depot update cost and shared content reuse; certify
the whole structure with `cavs certify workspace`.

## Use case: local testing server

```bash
cavs serve ./cavs-workspace --app my-app --branch beta --port 8990
```

Development/testing only (plain HTTP, localhost). It serves manifests,
bootstrap artifacts, chunks, packfile ranges, plans and signatures —
enough to test a launcher or the Godot plugin locally.

## Export a reproducible report

```bash
cavs certify export-repro \
  --certification ./certification \
  --out cavs-repro.tar.zst
```

The bundle includes commands, tool versions, environment metadata,
configs, report JSON + Markdown, and hashes. By default CAVS does
**not** include private build files; `--include-inputs` is for synthetic
or shareable test data only. See [REPRODUCIBILITY.md](REPRODUCIBILITY.md).

## Troubleshooting

**CAVS says butler is missing** — butler is optional; install it only
for external butler comparisons.

**bsdiff or xdelta3 is missing** — optional pairwise baselines; CAVS
skips them unless `--profile strict` marks a configured tool required.

**My compressed ZIP produces a huge update** — expected: global
compression destroys binary similarity. Analyze the uncompressed
folder: `cavs analyze steampipe ./Build_v1 ./Build_v2`.

**My PCK update is large** — `cavs analyze godot-pck old.pck new.pck`;
consider a secondary PCK for frequently updated content.

**Certification fails on hash mismatch** — do not ship the update.
Check for corrupted input, an incomplete previous artifact, a modified
cache, the wrong signature, or the wrong old/new build pair.

## FAQ

**Does CAVS require a CDN?** No — analysis, certification and offline
update plans run completely locally.

**Is CAVS a SteamPipe replacement?** No. The SteamPipe-style analysis is
a public model based on documentation, never Valve's implementation.

**Does CAVS replace butler?** No. It benchmarks against butler offline
when installed; it does not replace itch.io's pipeline.

**Does the Godot plugin change my PCK format?** No — the PCK is
reconstructed byte-for-byte and mounted normally.

**Is signing/encryption DRM?** No — they exist for integrity and
controlled testing workflows, not license enforcement.
