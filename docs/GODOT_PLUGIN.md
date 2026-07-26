# Godot plugin guide (v1.0.0)

The CAVS Godot plugin is one of several host integrations of the same core —
it delivers deduplicated updates to Godot 4 projects **without changing the
engine or the PCK format**. Any other host (a desktop app, an installer, a
service) embeds the same engine through `cavs-fetch`, an SDK or the C ABI; see
[EMBEDDABLE_FETCH.md](EMBEDDABLE_FETCH.md). It is 100% GDScript
(`addons/cavs/cavs_client.gd`, `class_name CavsClient`) — no
GDExtension, no native binaries — so it runs on every platform Godot
exports to. Full install and end-to-end instructions live in
[`game-engine-plugins/godot-plugin/README.md`](../game-engine-plugins/godot-plugin/README.md).

The plugin stays intentionally simple:

```text
1. download only what is needed (content-addressable cache in user://);
2. reconstruct the PCK;
3. verify the hash (SHA-256 per file, from the signed manifest);
4. mount it with ProjectSettings.load_resource_pack().
```

## The stable runtime API (frozen for v1.x)

```gdscript
var cavs := CavsClient.new("http://127.0.0.1:8990")
cavs.cache_dir = "user://cavs_cache"        # default

# Blocking (call from a Thread or a loading screen):
var result := cavs.fetch("main_pack")
if result.ok:
    ProjectSettings.load_resource_pack(result.files[0])

# Async — result delivered on the main thread:
cavs.fetch_async("main_pack", func(result): print(result.ok))

# One-liner: fetch + mount the asset's first .pck:
CavsClient.new("http://127.0.0.1:8990").ensure_pack("main_pack")
```

`fetch()` returns a Dictionary:
`{ ok: bool, error: String, files: Array[String], bytes_wire: int, … }`.
A `progress(done, total, stage)` signal drives loading bars. TLS with a
custom CA (`ca_cert_path`), retries and timeouts are configurable
fields; SHA-256 verification is on by default (`require_sha256`).

`cavs certify godot` verifies this surface stays exported
(`fetch`, `fetch_async`, `ensure_pack`) and fails certification if the
documented flow breaks — see NG9 in the v1.0.0 plan: no breaking
rewrite of the plugin API in v1.x.

## Certifying a PCK update before release

```bash
cavs certify godot \
  --old-pck old.pck \
  --new-pck new.pck \
  --out ./certification/godot
```

Checks:

- old/new PCK parse (Godot 4 `GDPC` directory, formats 1 and 2);
- `.cavsplan`, chunk/hybrid and bootstrap routes all reconstruct the
  new PCK **byte-identically** — mandatory, any mismatch fails;
- no route mounts an unverified PCK (every route hash-verifies before
  promotion);
- PCK analyzer report (`cavs analyze godot-pck`) with actionable layout
  recommendations;
- plugin API surface (with `--plugin-dir ./game-engine-plugins/godot-plugin/addons`);
- optional engine smoke test:

```bash
cavs certify godot \
  --old-pck old.pck --new-pck new.pck \
  --godot-bin /path/to/godot \
  --test-project ./godot-test-project \
  --out ./certification/godot
```

The smoke test runs `godot --headless --path <project> --quit` and is
skipped (never failed) when the binary or project is not provided.

## When PCK updates come out large

```bash
cavs analyze godot-pck old.pck new.pck --out godot-pck-analysis.md
```

Typical findings and fixes: shifted resource offsets after inserting
content early in the pack (split frequently-updated content into a
secondary PCK mounted on top), global compression destroying binary
similarity (prefer per-asset compression), and scene/texture churn
dragging whole packs (group volatile assets together). The base PCK +
add-on PCK pattern works naturally with `load_resource_pack()` mounting
order.
