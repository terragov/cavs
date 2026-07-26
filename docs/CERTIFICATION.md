# Release certification — `cavs certify` (v1.0.0)

`cavs certify` is the release-readiness command: it runs the CAVS
toolkit end-to-end for one build transition and answers, in one report,
whether the update is ready to publish:

- Is every output byte-identical?
- Which route should each client state use?
- Did performance regress against the previous release?
- Are pack layouts healthy?
- Does the Godot flow still work?
- Are the reports reproducible by someone else?

```bash
cavs certify --old ./Build_v1 --new ./Build_v2 --profile release --out ./certification
```

CAVS certifies updates **locally, before release**. It is not a
CDN, marketplace, SaaS, DRM system or distribution storefront, and its
SteamPipe-style figures are estimates from a public model — never
Valve's implementation.

## Command family

| Command | Purpose |
|---|---|
| `cavs certify` | Full orchestrated run (all sections below, per profile) |
| `cavs certify integrity` | Signatures, plans, apply, byte-identical, path safety, corruption smoke |
| `cavs certify routes` | Route selection per client state + measured route matrix |
| `cavs certify regressions` | Compare metrics against a baseline, fail on regression |
| `cavs certify godot` | Godot PCK flow: byte-identical on every route, analyzer, plugin API |
| `cavs certify workspace` | App/depot/branch/build metadata, previews, install plans, sharing |
| `cavs certify export-repro` | Deterministic reproducibility bundle (tar.zst) |

## Modes

**Artifact / directory / PCK pair** — point at two builds; kinds are
auto-detected, and a `.pck` pair (or `--engine godot`) also runs the
Godot section:

```bash
cavs certify --old ./Build_v1 --new ./Build_v2 --out ./certification/v1-to-v2
cavs certify --old old.pck --new new.pck --out ./certification/godot-pck
```

**Workspace transition** — certify a recorded build transition,
including per-depot integrity and install plans:

```bash
cavs certify \
  --workspace ./cavs-workspace \
  --app my-app \
  --from build_1001 \
  --to build_1002 \
  --out ./certification/build_1001_to_1002
```

**With external tools** (butler / xdelta3 / bsdiff are optional; missing
tools are skipped, never selected — under `--profile strict` a tool you
explicitly configure becomes required):

```bash
cavs certify \
  --old ./Build_v1 --new ./Build_v2 \
  --butler-bin ./butler --xdelta3-bin xdelta3 --bsdiff-bin bsdiff \
  --out ./certification/full
```

**With a regression baseline:**

```bash
cavs certify \
  --old ./Build_v1 --new ./Build_v2 \
  --baseline ./results/v0.9.0/baseline.json \
  --max-network-regression 5% \
  --max-apply-regression 10% \
  --out ./certification/release
```

## Profiles

```bash
cavs certify --profile quick|standard|release|strict|ci
```

| Check | quick | standard | release | strict | ci |
|---|---|---|---|---|---|
| integrity (byte-identical, path safety) | yes | yes | yes | yes | yes |
| no-op reapply | no | yes | yes | yes | yes |
| routes | estimate | measured | measured | measured | measured |
| SteamPipe-style + pack + I/O analysis | no | yes | yes | yes | yes |
| regressions (needs `--baseline`) | no | no | yes | yes | yes |
| Godot / workspace sections | no | no | when applicable | when applicable | when applicable |
| corruption smoke | no | no | no | yes | no |
| repro bundle by default | no | no | no | yes | no |
| configured external tools required | no | no | no | yes | no |

`--routes estimate` downgrades any profile to planner estimates (no
measured applies); `--export-repro <path>` adds the bundle to any
profile. Profiles are deterministic: the same profile always runs the
same checks.

## Exit codes (frozen for v1.x)

```text
0 = certification pass
1 = certification failed
2 = warning threshold exceeded, but outputs valid
3 = missing required dependency
4 = invalid input
5 = internal error
```

`--fail-on-warning` turns exit 2 into exit 1.

## Output directory

```text
certification/
  summary.md / summary.json          overall verdict, sections, metrics
  integrity.md / integrity.json      the integrity matrix
  routes.md / routes.json            per-state decisions + measured matrix
  regressions.md / regressions.json  when a baseline was given
  steampipe-style.md                 layout diagnosis
  pack-analysis.md                   pack churn/TOC/compression report
  io-estimate.md                     disk I/O per route and device
  godot.md / godot.json              PCK pairs only
  workspace.md / workspace.json      workspace mode only
  dependencies.json                  external tool availability + versions
  environment.json                   OS/CPU/RAM/tooling of the run
  commands.sh                        equivalent CLI commands, in order
  artifacts/
    old.cavssig / new.cavssig
    update.cavsplan
    route-results.json
    hashes.json                      BLAKE3 of inputs, plan and roots
  repro.tar.zst                      strict profile (or --export-repro)
```

## What each section checks

### Integrity (`cavs certify integrity`)

- `.cavssig` export, decode round-trip and verify against the source;
- `.cavsplan` build, decode round-trip (`CAVSPLAN1`);
- no path traversal: no absolute paths, `..` components or drive
  prefixes in plan entries or deletions;
- **apply output byte-identical** — verified against the new build's
  signature; any mismatch fails the certification, always;
- no-op reapply: re-applying the new→new plan in place rewrites 0 files
  (directory mode) / reconstructs from local bytes (artifact mode);
- corruption smoke (strict profile, and always in the standalone
  subcommand): a bit-flipped signature and plan must be rejected by
  their decoders, and a bit flipped inside a reused old range must
  never produce a verified output.

**Three different "no-op"s, certified in three different places.** They
are easy to conflate, so the reports keep them apart:

| Claim | What it means | Where it is certified |
|---|---|---|
| no-op **network** | a client that already has the new version re-fetches for ~0 bytes | routes section, `warm-cache` state (the online route planner resolves everything from cache) |
| no-op **reapply** | re-running the update against an already-updated install succeeds and stays byte-identical | integrity section, "no-op reapply" row |
| **no files rewritten** | that reapply also touches nothing on disk: directory applies detect every file as unchanged (0 written, N no-op) | integrity section, the same row's details |

For a single-artifact plan there is no per-file no-op: the artifact is
reconstructed from local bytes and the check instead asserts the plan
payload carries at most the file's sub-block tail (the block differ
always inlines the final partial block, even for identical inputs — a
plan file is therefore never literally 0 bytes).

Existing artifacts can be checked without the original bytes:

```bash
cavs certify integrity \
  --signature-old old.cavssig --signature-new new.cavssig \
  --plan update.cavsplan --out ./certification/integrity
```

### Routes (`cavs certify routes`)

See [ROUTE_SELECTION.md](ROUTE_SELECTION.md). Certifies the planner
decision for every documented client state (cold-install,
cold-cache-previous, warm-cache, exact-previous-version, low-ram,
slow-hdd, limited-disk), then measures every real route
(full/bootstrap/chunk-hybrid/plan, plus butler and bsdiff/xdelta3 when
installed) and fails if any measured output is not byte-identical.
`routes.json` carries the scores, the policy weights and the metrics
used by the regression guard.

### Regressions (`cavs certify regressions`)

Compares `metrics` between a current report and a baseline:

```bash
cavs certify regressions \
  --current ./certification/routes.json \
  --baseline ./results/v0.9.0/baseline.json \
  --max-network-regression 5% \
  --max-apply-regression 10% \
  --max-ram-regression 20% \
  --out ./certification/regressions
```

- byte-size metrics compare exactly against the network threshold —
  byte counts are deterministic for the same inputs, so any growth is
  real;
- `*_ms` metrics use the apply threshold **plus** a 250 ms absolute
  noise floor, and RAM metrics the RAM threshold plus a 32 MiB floor.
  A timing/RSS metric only fails when it exceeds *both* the relative
  threshold and the floor. Why: wall-clock and RSS jitter run-to-run
  even on the same machine — a 214 ms apply re-measured at 255 ms is
  +19%, which would fail a 10% threshold on pure noise. The floor makes
  small workloads immune to jitter while still catching real
  regressions on workloads where timing matters (seconds and up);
- losing byte-identical status always fails, no exceptions;
- `--allow-regression metric=reason` accepts a named regression with an
  explicit reason (reported as a warning, exit 2);
- record a baseline with `cavs certify --save-baseline baseline.json`.

Baseline schema (`cavs-certify-baseline/1`):

```json
{
  "schema": "cavs-certify-baseline/1",
  "cavs_version": "1.0.0",
  "byte_identical": true,
  "metrics": { "network_bytes": 2373505.0, "apply_ms": 214.0 }
}
```

### Godot (`cavs certify godot`)

See [GODOT_PLUGIN.md](GODOT_PLUGIN.md). PCK parse, byte-identical
reconstruction on the plan / chunk-hybrid / bootstrap routes, the PCK
analyzer report, the plugin API surface check (`fetch`, `fetch_async`,
`ensure_pack` must stay exported) and an optional engine smoke test:

```bash
cavs certify godot \
  --old-pck old.pck --new-pck new.pck \
  --godot-bin /path/to/godot --test-project ./godot-test-project \
  --plugin-dir ./game-engine-plugins/godot-plugin/addons \
  --out ./certification/godot
```

The Godot binary and test project are optional; the smoke test is
skipped (not failed) without them. No route ever mounts an unverified
PCK — every route hash-verifies its output before promotion.

### Workspace (`cavs certify workspace`)

Validates workspace metadata, app/depots/branches/builds, promote and
rollback previews (rollback may only target builds the branch served
before), deterministic depot-sharing math, per-depot update cost, and
install plans per platform/language/ownership derived from the app's
depots. In full workspace-mode runs, each depot present in both builds
is also integrity-certified.

### Reproducibility (`cavs certify export-repro`)

See [REPRODUCIBILITY.md](REPRODUCIBILITY.md). Deterministic tar.zst with
commands, environment, tool versions, reports, report hashes and input
hashes — never the input bytes unless `--include-inputs` is passed
explicitly.

## JSON schemas

Every JSON report carries a `schema` field, frozen for v1.x (changes
are additive):

```text
cavs-certify-summary/1      summary.json
cavs-certify-integrity/1    integrity.json
cavs-certify-routes/1       routes.json
cavs-certify-regressions/1  regressions.json
cavs-certify-godot/1        godot.json
cavs-certify-workspace/1    workspace.json
cavs-certify-baseline/1     baseline files
```

`summary.json` headline fields: `result`
(`pass|warn|fail|skipped`), `profile`, `mode`, `old`, `new`,
`recommended_route`, `reason[]`, `sections[]` (name/result/rows),
`metrics{}`, `byte_identical`, `exit_code`.

## CI

See [CI.md](CI.md) for GitHub Actions / GitLab CI recipes built on the
`ci` profile, `--json-out` and the stable exit codes.
