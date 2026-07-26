# Reproducible certification bundles (v1.0.0)

Every CAVS certification can be exported as a bundle another developer
can use to verify the numbers:

```bash
cavs certify export-repro \
  --certification ./certification/v1-to-v2 \
  --out repro-v1-to-v2.tar.zst
```

or in one pass:

```bash
cavs certify --old ./Build_v1 --new ./Build_v2 \
  --export-repro repro.tar.zst --out ./certification/release
```

The `strict` profile exports `certification/repro.tar.zst` by default.

## Bundle contents

```text
repro/
  README.md                      how to verify this certification
  commands.sh                    every command of the run, in order
  environment.json               OS, CPU, RAM, disk, cavs + tool versions
  tool-versions.json             external tool availability + versions
  inputs/
    hashes.json                  BLAKE3 of old/new inputs, plan, merkle roots
    file-list.json               artifact names and sizes
  outputs/
    summary.json / routes.json / integrity.json / regressions.json / …
    report-hashes.json           BLAKE3 per bundled report
  reports/
    summary.md / routes.md / integrity.md / …
  configs/                       cavs.toml / policy.toml when present
  seeds/dataset-seed.txt         when the inputs were synthetic
```

## Guarantees

- **Deterministic**: entries are sorted, timestamps zeroed, owners
  zeroed; exporting the same certification twice produces byte-identical
  bundles.
- **No private inputs by default**: the bundle carries hashes and file
  lists, never the input builds. `--include-inputs` embeds them — use it
  only for synthetic or shareable test data, never a private build.
- **Self-describing**: `commands.sh` + `environment.json` +
  `tool-versions.json` are everything needed to re-run the
  certification; `outputs/report-hashes.json` lets a verifier confirm
  the reports were not altered after the fact.

## Verifying someone else's bundle

```bash
zstd -dc repro.tar.zst | tar -x
cat repro/README.md              # instructions
cat repro/inputs/hashes.json     # confirm you have the same inputs
sh repro/commands.sh             # re-run against your copies
diff -r ./certification repro/reports   # compare (timings will jitter)
```

Byte counts (network, plan, signature sizes) reproduce exactly for the
same inputs; wall-clock timings reproduce within normal jitter — the
regression guard's noise floors exist for exactly that reason
([CERTIFICATION.md](CERTIFICATION.md)).
