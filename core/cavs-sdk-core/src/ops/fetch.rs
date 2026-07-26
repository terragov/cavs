//! `fetchStatic` — install or update a build straight from a static export
//! (`cavs store export --static-plans`) with no cavs-server, in-process. This
//! is the operation a launcher, installer or application embeds (via the SDKs or the C ABI, which
//! is what the Unity and Unreal plugins call) to self-update: it plans the
//! missing set against a persistent cache and downloads only what changed,
//! concurrently, reporting progress and honouring cancellation.

use crate::error::{Result, SdkError};
use crate::progress::OpCtx;
use cavs_fetch::{fetch_static, FetchError, FetchOptions, StaticSource};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FetchStaticRequest {
    /// Base URL or local directory of the static export.
    base: String,
    /// Asset name (`<name>` under `assets/<name>/`).
    asset: String,
    /// Output directory for the reconstructed build.
    output_dir: PathBuf,
    /// Persistent content-addressable cache directory.
    cache_dir: PathBuf,
    /// Concurrent range requests.
    #[serde(default = "default_connections")]
    connections: usize,
    /// Optional Ed25519 public key (64 hex) to enforce the content signature.
    #[serde(default)]
    pubkey: Option<String>,
}

fn default_connections() -> usize {
    8
}

pub fn run(ctx: &OpCtx, request: &Value) -> Result<Value> {
    let req: FetchStaticRequest = serde_json::from_value(request.clone())
        .map_err(|e| SdkError::InvalidRequest(e.to_string()))?;

    ctx.phase("planning");
    ctx.check_cancelled()?;

    let source = StaticSource::new(&req.base);
    // Bridge the SDK progress/cancellation into the fetch engine.
    let progress = |done: u64, total: u64| ctx.bytes("downloading", done, total, None);
    let opts = FetchOptions {
        connections: req.connections,
        pubkey: req.pubkey.clone(),
        progress: Some(&progress),
        cancel: ctx.cancel,
    };

    let stats = fetch_static(&source, &req.asset, &req.output_dir, &req.cache_dir, &opts)
        .map_err(map_fetch_err)?;

    Ok(json!({
        "asset": req.asset,
        "outputDir": req.output_dir,
        "wireBytes": stats.wire_bytes,
        "rawBytes": stats.raw_bytes,
        "chunksFetched": stats.fetched,
        "chunksReused": stats.reused,
        "logicalBytes": stats.logical_bytes,
        "savedPercent": if stats.logical_bytes == 0 {
            0.0
        } else {
            (stats.logical_bytes.saturating_sub(stats.wire_bytes)) as f64 * 100.0
                / stats.logical_bytes as f64
        },
    }))
}

fn map_fetch_err(e: FetchError) -> SdkError {
    match e {
        FetchError::Cancelled => SdkError::Cancelled,
        FetchError::Other(e) => SdkError::Io(std::io::Error::other(format!("{e:#}"))),
    }
}
