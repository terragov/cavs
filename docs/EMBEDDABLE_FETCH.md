# Embeddable fetch engine (`cavs-fetch`, v1.4.0)

`cavs-fetch` is the serverless install/update path as a **library**, so a
launcher, installer or application can self-update **in-process** — no `cavs-server`, no
shelling out to the CLI, no re-implementation of the protocol. It is exposed
three ways:

- as a Rust crate (`cavs-fetch`),
- as the SDK operation **`fetchStatic`** (Go, Kotlin, Node),
- through the C ABI (`libcavs_sdk`), which is what the Unity and Unreal plugins
  call.

## What it does

Given a static export (see [SERVERLESS_DELIVERY.md](SERVERLESS_DELIVERY.md))
and a persistent cache directory, it:

1. reads the asset's `manifest.json` and `chunk-map.json`,
2. computes the missing chunk set against the cache,
3. downloads the missing chunks concurrently (HTTP Range for a URL base, slice
   reads for a local directory), decompressing and BLAKE3-verifying each,
4. reconstructs the output files (`.part` → SHA-256 verify → atomic rename).

It reports progress through a callback and supports cooperative cancellation,
and can enforce an Ed25519 content signature.

## Rust

```rust
use cavs_fetch::{fetch_static, FetchOptions, StaticSource};
use std::sync::atomic::AtomicBool;

let source = StaticSource::new("https://cdn.example.com/dist"); // or a local dir
let cancel = AtomicBool::new(false);
let on_progress = |done: u64, total: u64| {
    println!("{:.0}%", if total == 0 { 100.0 } else { done as f64 * 100.0 / total as f64 });
};
let opts = FetchOptions {
    connections: 8,
    pubkey: None,
    progress: Some(&on_progress),
    cancel: Some(&cancel),
};
let stats = fetch_static(&source, "app", "./install".as_ref(), "./cache".as_ref(), &opts)?;
println!("fetched {}, reused {}, saved {:.1}%",
    stats.fetched, stats.reused,
    100.0 - stats.wire_bytes as f64 * 100.0 / stats.logical_bytes as f64);
```

## SDK (`fetchStatic`)

All three SDKs expose the operation with the same fields; progress and
cancellation flow through each SDK's usual mechanism.

```ts
// Node
const cavs = new CavsClient();
const r = await cavs.fetchStatic(
  { base: "https://cdn.example.com/dist", asset: "app",
    outputDir: "./install", cacheDir: "./cache", connections: 8 },
  { onProgress: e => console.log(e.percentage) });
console.log(r.chunksFetched, r.chunksReused, r.savedPercent);
```

```go
// Go
r, _ := client.FetchStatic(ctx, cavs.FetchStaticRequest{
    Base: "https://cdn.example.com/dist", Asset: "app",
    OutputDir: "./install", CacheDir: "./cache", Connections: 8,
})
```

```kotlin
// Kotlin
val r = cavs.fetchStatic(FetchStaticRequest(
    base = "https://cdn.example.com/dist", asset = "app",
    outputDir = "./install", cacheDir = "./cache", connections = 8))
```

## C ABI

`fetchStatic` needs no new ABI: the engine is generic JSON-in/out. Start it as
an async job so a UI thread stays responsive, poll to completion, and drain the
progress callback:

```c
CavsContext *ctx = cavs_context_new("{}");
cavs_context_set_progress_callback(ctx, on_progress, user_data);
const char *req =
  "{\"schemaVersion\":\"1.0\",\"data\":{"
  "\"base\":\"https://cdn.example.com/dist\",\"asset\":\"app\","
  "\"outputDir\":\"./install\",\"cacheDir\":\"./cache\",\"connections\":8}}";
CavsJob *job = cavs_start_json(ctx, "fetchStatic", req);
CavsResult *res;
while ((res = cavs_job_poll(job)) == NULL) { /* pump UI; cavs_job_cancel(job) to abort */ }
if (cavs_result_ok(res)) { /* parse cavs_result_json(res) */ }
cavs_result_free(res); cavs_job_free(job); cavs_context_free(ctx);
```

The Unity plugin (`game-engine-plugins/unity-plugin`, C# P/Invoke) and the
Unreal plugin (`game-engine-plugins/unreal-plugin`, `UCavsClient` in C++) wrap
exactly this flow. **Both plugins are currently untested** — see their
READMEs.

## Result fields

`fetchStatic` returns: `chunksFetched`, `chunksReused`, `wireBytes`
(downloaded, possibly compressed), `rawBytes` (decompressed), `logicalBytes`
(total build size) and `savedPercent`.

## Cache interop

The cache layout (`<root>/<ab>/<hex>`, raw payloads) is identical to the
`cavs-client` cache, so an application embedding `cavs-fetch` and the CLI can share one
cache directory.
