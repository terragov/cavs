# CAVS certification in CI (v1.0.0)

`cavs certify` is CI-friendly by design: stable exit codes, a stable
JSON schema, no TTY requirements, and a report directory that uploads
cleanly as a CI artifact.

```bash
cavs certify \
  --old ./previous-build \
  --new ./current-build \
  --profile ci \
  --json-out certification.json \
  --fail-on-warning \
  --out certification
```

- `--profile ci` = the release checks with machine-readable output
  (integrity, measured routes, analysis, regression guard when a
  baseline is given).
- `--json-out` duplicates `summary.json` wherever your pipeline expects
  it.
- `--fail-on-warning` turns exit 2 (warnings) into exit 1 for gates
  that must be strict.

## Exit codes

```text
0 = pass          1 = failed        2 = warnings (outputs valid)
3 = missing dep   4 = invalid input 5 = internal error
```

Gate on `!= 0` for strict pipelines, or on `== 1 || >= 3` if layout
warnings should not block merges.

## GitHub Actions

```yaml
name: CAVS Certification

on:
  pull_request:
  workflow_dispatch:

jobs:
  certify:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Build artifact
        run: ./scripts/export_game.sh

      - name: Fetch previous release build
        run: ./scripts/fetch_previous_build.sh ./previous-build

      - name: Run CAVS certification
        run: |
          cavs certify \
            --old ./previous-build \
            --new ./current-build \
            --profile ci \
            --baseline ./baselines/latest.json \
            --json-out certification.json \
            --out certification

      - name: Upload CAVS reports
        uses: actions/upload-artifact@v4
        if: always()
        with:
          name: cavs-certification
          path: certification/
```

## GitLab CI

```yaml
cavs-certify:
  stage: test
  script:
    - ./scripts/export_game.sh
    - ./scripts/fetch_previous_build.sh ./previous-build
    - cavs certify --old ./previous-build --new ./current-build
        --profile ci --baseline baselines/latest.json
        --json-out certification.json --out certification
  artifacts:
    when: always
    paths: [certification/]
```

## Regression baselines in CI

Record a baseline when you cut a release and commit it (or store it as
a pipeline artifact):

```bash
cavs certify --old ./v0_build --new ./v1_build \
  --profile release --save-baseline baselines/v1.json --out certification
```

Subsequent runs pass `--baseline baselines/v1.json`; thresholds are
`--max-network-regression 5%`, `--max-apply-regression 10%`,
`--max-ram-regression 20%` by default. Accept a known, justified
regression without weakening the gate:

```bash
--allow-regression "network_bytes=asset repack for the new level, accepted in #142"
```

## Optional dependencies

butler / xdelta3 / bsdiff are never required by the `ci` profile —
missing tools are recorded in `dependencies.json` and their routes are
skipped. If your pipeline *must* compare against butler, pass
`--butler-bin` under `--profile strict`, which turns configured tools
into hard requirements (exit 3 when missing).

## Reading the results

`certification.json` (`cavs-certify-summary/1`):

```json
{
  "result": "pass",
  "exit_code": 0,
  "recommended_route": "CAVS offline plan (.cavsplan)",
  "byte_identical": true,
  "sections": [ { "name": "Integrity", "result": "pass", "rows": [ … ] } ],
  "metrics": { "network_bytes": 2373513.0, "apply_ms": 214.0, … }
}
```

Post `summary.md` as a PR comment for humans; parse `summary.json` for
gates and dashboards.
