//! `cavs-fetch` — an embeddable serverless/CDN fetch engine for CAVS.
//!
//! A launcher, installer or application links this library to install and update a build
//! **in-process**, straight from a static export produced by
//! `cavs store export --static-plans` (S3 / R2 / GitHub Pages / nginx / a
//! local folder) — no `cavs-server` and no shelling out to the CLI. It:
//!
//! 1. reads the per-asset `manifest.json` (reconstruction structure) and
//!    `chunk-map.json` (each chunk's pack + absolute byte range),
//! 2. computes the missing set against a persistent content-addressable
//!    cache (so an update downloads only what changed),
//! 3. HTTP-Range-GETs (or slice-reads, for a local folder) the missing
//!    chunks concurrently, verifying every one by BLAKE3, and
//! 4. reconstructs the output files from the cache — byte-identical or it
//!    fails.
//!
//! It reports progress through a callback and supports cooperative
//! cancellation, so a UI can show a progress bar and a Cancel button. The
//! same engine is exposed through the CAVS SDKs (`fetchStatic`) and the C
//! ABI, which is what the Unity and Unreal plugins call.

mod adaptive;
mod cache;
mod meta;
mod reconstruct;
mod source;

pub use cache::ChunkCache;
pub use meta::{MetaStats, MetadataResolver};
pub use source::StaticSource;

use adaptive::{AimdController, Gate, INITIAL_CONCURRENCY, MAX_CONCURRENCY, MIN_CONCURRENCY};
use anyhow::{bail, Context, Result};
use cavs_hash::{from_hex, hash_chunk, ChunkHash};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

const CHUNK_FLAG_ZSTD: u32 = 1;

/// BG4 pretransform chunk flag (mirrors `cavs_format::CHUNK_FLAG_BG4`).
const CHUNK_FLAG_BG4: u32 = 1 << 1;

/// Inverse of the BG4 byte-grouping pretransform (mirrors
/// `cavs_format::bg4_ungroup`; duplicated to keep this crate embeddable
/// without a cavs-format dependency).
fn bg4_ungroup(grouped: &[u8]) -> Vec<u8> {
    let len = grouped.len();
    let mut out = vec![0u8; len];
    let mut it = grouped.iter();
    for lane in 0..4 {
        let mut i = lane;
        while i < len {
            out[i] = *it.next().unwrap();
            i += 4;
        }
    }
    out
}

/// Options for a serverless fetch.
pub struct FetchOptions<'a> {
    /// Concurrent range requests. `0` = adaptive (AUTO): an AIMD controller
    /// starts at 8 connections and moves between 2 and 64 — +1 per clean
    /// one-second window, halved (with a 1 s cooldown) on pressure from the
    /// remote (failed range request, short read, HTTP 429/503). `>= 1` = a
    /// fixed pool of exactly that many connections, the historical behavior.
    /// The `CAVS_FETCH_CONCURRENCY` env var (`auto` or an integer) overrides
    /// this field when set, so operators can tune deployed embedders.
    pub connections: usize,
    /// Optional Ed25519 public key (64 hex) to enforce the content signature.
    pub pubkey: Option<String>,
    /// Progress callback: invoked with cumulative `(done_bytes, total_bytes)`
    /// as chunks land. `total_bytes` is the wire size of the missing set.
    pub progress: Option<&'a (dyn Fn(u64, u64) + Send + Sync)>,
    /// Cooperative cancellation: when set to `true`, an in-flight fetch stops
    /// and returns [`FetchError::Cancelled`].
    pub cancel: Option<&'a AtomicBool>,
}

impl Default for FetchOptions<'_> {
    fn default() -> Self {
        Self {
            connections: 8,
            pubkey: None,
            progress: None,
            cancel: None,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct FetchStats {
    /// Bytes pulled over the wire (stored, possibly compressed).
    pub wire_bytes: u64,
    /// Decompressed bytes written to the cache.
    pub raw_bytes: u64,
    /// Chunks downloaded.
    pub fetched: u64,
    /// Chunks already present in the cache (an update's reuse).
    pub reused: u64,
    /// Total logical size of the reconstructed asset.
    pub logical_bytes: u64,
    /// Range requests issued (including retries).
    pub requests: u64,
    /// Stored bytes of the chunks actually needed; `wire_bytes /
    /// useful_bytes` is the coalescing's read amplification.
    pub useful_bytes: u64,
    /// Chunks re-fetched individually after failing verification inside a
    /// coalesced range (selective retry, not a whole-range repeat).
    pub selective_retries: u64,
    /// Times a worker had to wait on the global inflight-byte budget.
    pub throttle_waits: u64,
    /// Metadata requests this fetch issued (manifest/chunk-map/meta-pack);
    /// 0 when everything came from the resolver's caches.
    pub metadata_requests: u64,
    /// Wall time resolving metadata, in milliseconds.
    pub metadata_ms: u64,
    /// Wall time planning ranges (missing-set + coalescing), in ms.
    pub plan_ms: u64,
    /// Wall time downloading + decoding + caching payload, in ms.
    pub payload_ms: u64,
    /// Wall time reconstructing the output from the cache, in ms.
    pub reconstruct_ms: u64,
    /// Concurrency mode the payload phase ran with: 0 = fixed pool,
    /// 1 = adaptive (AIMD). A plain integer, not an enum, to keep the
    /// struct `Copy` and flat for the C ABI / JSON stats surfaces.
    pub concurrency_mode: u64,
    /// High-water mark of workers concurrently inside the download section
    /// (the pool size itself in fixed mode).
    pub concurrency_peak: u64,
    /// AIMD multiplicative decreases triggered by pressure events; always 0
    /// in fixed mode. A nonzero value means the remote pushed back.
    pub aimd_decreases: u64,
}

/// A fetch failure with a stable reason, so an embedder can decide
/// retry/repair/cancel without parsing prose.
#[derive(Debug)]
pub enum FetchError {
    Cancelled,
    Other(anyhow::Error),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Cancelled => write!(f, "CAVS-E-CANCELLED: fetch cancelled"),
            FetchError::Other(e) => write!(f, "{e:#}"),
        }
    }
}
impl std::error::Error for FetchError {}
impl From<anyhow::Error> for FetchError {
    fn from(e: anyhow::Error) -> Self {
        FetchError::Other(e)
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChunkMapFile {
    #[allow(dead_code)]
    pub(crate) asset: String,
    pub(crate) chunks: Vec<ChunkMapEntry>,
}

#[derive(Debug, Deserialize, serde::Serialize, Clone)]
pub(crate) struct ChunkMapEntry {
    pub(crate) hash: String,
    pub(crate) len_raw: u32,
    pub(crate) len_stored: u32,
    pub(crate) flags: u32,
    pub(crate) pack: String,
    pub(crate) pack_offset_abs: u64,
}

/// Fetch `asset` from the static tree at `source` into `output`, caching in
/// `cache_dir`. Returns egress/reuse stats. Byte-identical reconstruction or
/// an error — a partially written output file is never promoted.
pub fn fetch_static(
    source: &StaticSource,
    asset: &str,
    output: &Path,
    cache_dir: &Path,
    opts: &FetchOptions,
) -> std::result::Result<FetchStats, FetchError> {
    // An ephemeral resolver still gets meta-pack batching and the on-disk
    // L2 cache; only the in-process L1 is lost across calls. Long-lived
    // embedders should hold a [`MetadataResolver`] and use
    // [`fetch_static_with_resolver`].
    let resolver = MetadataResolver::new(cache_dir);
    fetch_static_with_resolver(source, asset, output, cache_dir, opts, &resolver)
}

/// [`fetch_static`] with a caller-held [`MetadataResolver`], so a session
/// fetching many assets from one remote shares its metadata caches and
/// meta-pack prefetches across objects.
pub fn fetch_static_with_resolver(
    source: &StaticSource,
    asset: &str,
    output: &Path,
    cache_dir: &Path,
    opts: &FetchOptions,
    resolver: &MetadataResolver,
) -> std::result::Result<FetchStats, FetchError> {
    fetch_static_inner(source, asset, output, cache_dir, opts, resolver).map_err(|e| {
        // Preserve an explicit cancellation as such.
        if e.downcast_ref::<Cancelled>().is_some() {
            FetchError::Cancelled
        } else {
            FetchError::Other(e)
        }
    })
}

struct Cancelled;
impl std::fmt::Debug for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cancelled")
    }
}
impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cancelled")
    }
}
impl std::error::Error for Cancelled {}

fn fetch_static_inner(
    source: &StaticSource,
    asset: &str,
    output: &Path,
    cache_dir: &Path,
    opts: &FetchOptions,
    resolver: &MetadataResolver,
) -> Result<FetchStats> {
    let cache = ChunkCache::open(cache_dir)?;

    // 1. Metadata: manifest + chunk locations, through the batching
    //    resolver (meta-packs, L1/L2 caches, per-asset fallback).
    let t = std::time::Instant::now();
    let meta_before = resolver.stats().requests;
    let meta = resolver.resolve(source, asset)?;
    let metadata_requests = resolver.stats().requests - meta_before;
    let metadata_ms = t.elapsed().as_millis() as u64;
    let manifest = &meta.manifest;

    if let Some(pk) = &opts.pubkey {
        verify_signature(manifest, pk)?;
    }

    // 2. Missing set.
    let t = std::time::Instant::now();
    let locations: HashMap<&str, &ChunkMapEntry> =
        meta.chunks.iter().map(|c| (c.hash.as_str(), c)).collect();
    let mut seen = std::collections::HashSet::new();
    let mut missing: Vec<ChunkMapEntry> = Vec::new();
    let mut reused = 0u64;
    for hex in manifest_chunk_hashes(manifest) {
        if !seen.insert(hex.clone()) {
            continue;
        }
        if cache.contains(&hex) {
            reused += 1;
            continue;
        }
        let loc = locations.get(hex.as_str()).with_context(|| {
            format!("chunk {hex} referenced by manifest but absent from chunk-map")
        })?;
        missing.push((*loc).clone());
    }

    // 3. Concurrent range fetch, coalesced: adjacent missing chunks of the
    //    same pack travel in one Range GET instead of one request per chunk.
    let groups = plan_range_groups(missing);
    let plan_ms = t.elapsed().as_millis() as u64;
    let t = std::time::Instant::now();
    let total_wire: u64 = groups.iter().map(|g| g.span).sum();
    let stats = fetch_missing_parallel(source, &groups, &cache, opts, total_wire)?;
    let payload_ms = t.elapsed().as_millis() as u64;

    // 4. Reconstruct from cache.
    let t = std::time::Instant::now();
    reconstruct::reconstruct(manifest, &cache, output)?;
    let reconstruct_ms = t.elapsed().as_millis() as u64;

    Ok(FetchStats {
        reused,
        logical_bytes: reconstruct::logical_bytes(manifest),
        metadata_requests,
        metadata_ms,
        plan_ms,
        payload_ms,
        reconstruct_ms,
        ..stats
    })
}

/// Tolerated gap between two missing chunks fetched in one range: the extra
/// bytes cost less than another round-trip (mirrors the store's read
/// coalescing).
const MAX_COALESCE_GAP: u64 = 64 * 1024;

/// Upper bound of one coalesced range: keeps per-request memory bounded and
/// requests parallelizable across connections.
const MAX_COALESCED_RANGE: u64 = 8 * 1024 * 1024;

/// Amplification guard: gap bytes a group may accumulate, as a fraction of
/// its useful (chunk) bytes. Coalescing trades wasted bytes for saved
/// round-trips; this caps the trade so a sparse update never downloads
/// multiples of what it needs (15% ≈ amplification ≤ 1.15× per group).
const MAX_WASTE_RATIO_PCT: u64 = 15;

/// Process-wide ceiling on wire bytes in flight across every concurrent
/// fetch, so N simultaneous downloads can't stack N × connections × 8 MiB
/// of buffers. Override with `CAVS_FETCH_MAX_INFLIGHT_BYTES`.
const DEFAULT_MAX_INFLIGHT_BYTES: u64 = 128 * 1024 * 1024;

/// Weighted semaphore over inflight wire bytes (backpressure). A worker
/// acquires its group's span before the range request and releases it once
/// the chunks are decoded and cached; requests larger than the whole budget
/// are clamped so they still run (serialized) instead of deadlocking.
struct ByteBudget {
    max: u64,
    used: Mutex<u64>,
    freed: std::sync::Condvar,
}

impl ByteBudget {
    fn global() -> &'static ByteBudget {
        static BUDGET: std::sync::OnceLock<ByteBudget> = std::sync::OnceLock::new();
        BUDGET.get_or_init(|| {
            let max = std::env::var("CAVS_FETCH_MAX_INFLIGHT_BYTES")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(DEFAULT_MAX_INFLIGHT_BYTES);
            ByteBudget {
                max,
                used: Mutex::new(0),
                freed: std::sync::Condvar::new(),
            }
        })
    }

    /// Block until `n` bytes fit in the budget. Returns whether the caller
    /// had to wait (a backpressure event, surfaced in [`FetchStats`]).
    fn acquire(&self, n: u64) -> (BudgetPermit<'_>, bool) {
        let n = n.min(self.max);
        let mut used = self.used.lock().unwrap();
        let mut waited = false;
        while *used + n > self.max {
            waited = true;
            used = self.freed.wait(used).unwrap();
        }
        *used += n;
        (BudgetPermit { budget: self, n }, waited)
    }
}

struct BudgetPermit<'a> {
    budget: &'a ByteBudget,
    n: u64,
}

impl Drop for BudgetPermit<'_> {
    fn drop(&mut self) {
        *self.budget.used.lock().unwrap() -= self.n;
        self.budget.freed.notify_all();
    }
}

/// One Range GET covering a run of missing chunks in the same pack.
struct RangeGroup {
    pack: String,
    /// Absolute offset of the first chunk.
    start: u64,
    /// Bytes to request (last chunk end − start, gaps included).
    span: u64,
    /// Chunk payload bytes inside the span (span − useful = waste).
    useful: u64,
    chunks: Vec<ChunkMapEntry>,
}

/// Group the missing set into coalesced ranges: sort by (pack, offset), then
/// extend the current run while the gap to the next chunk is at most
/// [`MAX_COALESCE_GAP`], the total span stays within
/// [`MAX_COALESCED_RANGE`], **and** the group's accumulated gap bytes stay
/// under [`MAX_WASTE_RATIO_PCT`] of its useful bytes. A push writes related
/// chunks contiguously, so a cold or update fetch typically collapses
/// thousands of per-chunk requests into a few dozen ranges — while the
/// waste cap keeps a sparse update's read amplification bounded.
fn plan_range_groups(mut missing: Vec<ChunkMapEntry>) -> Vec<RangeGroup> {
    missing.sort_by(|a, b| {
        (a.pack.as_str(), a.pack_offset_abs).cmp(&(b.pack.as_str(), b.pack_offset_abs))
    });
    let mut groups: Vec<RangeGroup> = Vec::new();
    for entry in missing {
        let end = entry.pack_offset_abs + entry.len_stored as u64;
        if let Some(g) = groups.last_mut() {
            let g_end = g.start + g.span;
            if g.pack == entry.pack && entry.pack_offset_abs >= g_end {
                let useful = g.useful + entry.len_stored as u64;
                let span = end - g.start;
                if entry.pack_offset_abs - g_end <= MAX_COALESCE_GAP
                    && span <= MAX_COALESCED_RANGE
                    && (span - useful) * 100 <= useful * MAX_WASTE_RATIO_PCT
                {
                    g.span = span;
                    g.useful = useful;
                    g.chunks.push(entry);
                    continue;
                }
            }
        }
        groups.push(RangeGroup {
            pack: entry.pack.clone(),
            start: entry.pack_offset_abs,
            span: entry.len_stored as u64,
            useful: entry.len_stored as u64,
            chunks: vec![entry],
        });
    }
    groups
}

/// The one operation [`fetch_group`] needs from a source; a trait so tests
/// can inject transient failures and stale bytes.
trait RangeSource: Sync {
    fn get_range(&self, rel: &str, offset: u64, len: u64) -> Result<Vec<u8>>;
}

impl RangeSource for StaticSource {
    fn get_range(&self, rel: &str, offset: u64, len: u64) -> Result<Vec<u8>> {
        StaticSource::get_range(self, rel, offset, len)
    }
}

/// Per-group result counters, aggregated into [`FetchStats`].
#[derive(Debug, Default)]
struct GroupOutcome {
    raw: usize,
    wire: usize,
    chunks: usize,
    requests: usize,
    selective_retries: usize,
}

/// Resolve the effective `connections` value: an explicit
/// `CAVS_FETCH_CONCURRENCY` env value (`auto` → 0, or an integer) beats the
/// caller's `FetchOptions`; anything unparsable falls back to the caller's
/// value rather than silently changing the mode. Pure, so it is testable
/// without mutating process state.
fn concurrency_override(env: Option<&str>, fallback: usize) -> usize {
    match env {
        Some(v) if v.trim().eq_ignore_ascii_case("auto") => 0,
        Some(v) => v.trim().parse::<usize>().unwrap_or(fallback),
        None => fallback,
    }
}

fn fetch_missing_parallel(
    source: &dyn RangeSource,
    missing: &[RangeGroup],
    cache: &ChunkCache,
    opts: &FetchOptions,
    total_wire: u64,
) -> Result<FetchStats> {
    let env = std::env::var("CAVS_FETCH_CONCURRENCY").ok();
    let connections = concurrency_override(env.as_deref(), opts.connections);
    let auto = connections == 0;
    if missing.is_empty() {
        if let Some(p) = opts.progress {
            p(0, 0);
        }
        return Ok(FetchStats {
            concurrency_mode: auto as u64,
            ..FetchStats::default()
        });
    }
    // AUTO sizes the pool once at the ceiling and lets the gate park the
    // workers above the AIMD limit; fixed mode keeps the historical exact
    // pool with no gate at all.
    let adaptive: Option<(AimdController, Gate)> = auto.then(|| {
        (
            AimdController::new(MIN_CONCURRENCY, INITIAL_CONCURRENCY, MAX_CONCURRENCY),
            Gate::new(),
        )
    });
    let workers = if auto {
        MAX_CONCURRENCY.min(missing.len())
    } else {
        connections.max(1).min(missing.len())
    };
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let first_error: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    let wire = AtomicUsize::new(0);
    let raw = AtomicUsize::new(0);
    let fetched = AtomicUsize::new(0);
    let requests = AtomicUsize::new(0);
    let selective = AtomicUsize::new(0);
    let throttled = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                if failed.load(Ordering::Relaxed) {
                    return;
                }
                if opts.cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                    let mut g = first_error.lock().unwrap();
                    if g.is_none() {
                        *g = Some(anyhow::Error::new(Cancelled));
                    }
                    failed.store(true, Ordering::Relaxed);
                    return;
                }
                // AUTO: hold one of the AIMD limit's slots for the whole
                // download+decode of this group. The slot is taken BEFORE
                // claiming a work item — a parked worker must never hold
                // an item hostage while admitted workers idle. A waiter
                // gives up (`None`) when another worker latched a failure.
                let ctrl = adaptive.as_ref().map(|(c, _)| c);
                let _slot = match &adaptive {
                    Some((c, gate)) => {
                        match gate.enter(|| c.limit(), || failed.load(Ordering::Relaxed)) {
                            Some(slot) => Some(slot),
                            None => return,
                        }
                    }
                    None => None,
                };
                let idx = next.fetch_add(1, Ordering::Relaxed);
                if idx >= missing.len() {
                    return;
                }
                let group = &missing[idx];
                let (_permit, waited) = ByteBudget::global().acquire(group.span);
                if waited {
                    throttled.fetch_add(1, Ordering::Relaxed);
                }
                match fetch_group(source, group, cache, ctrl) {
                    Ok(out) => {
                        wire.fetch_add(out.wire, Ordering::Relaxed);
                        raw.fetch_add(out.raw, Ordering::Relaxed);
                        fetched.fetch_add(out.chunks, Ordering::Relaxed);
                        requests.fetch_add(out.requests, Ordering::Relaxed);
                        selective.fetch_add(out.selective_retries, Ordering::Relaxed);
                        if let Some(c) = ctrl {
                            c.on_success();
                        }
                        if let Some(p) = opts.progress {
                            p(wire.load(Ordering::Relaxed) as u64, total_wire);
                        }
                    }
                    Err(e) => {
                        if let Some(c) = ctrl {
                            c.on_pressure();
                        }
                        let mut g = first_error.lock().unwrap();
                        if g.is_none() {
                            *g = Some(e);
                        }
                        failed.store(true, Ordering::Relaxed);
                        return;
                    }
                }
            });
        }
    });

    if let Some(e) = first_error.into_inner().unwrap() {
        return Err(e);
    }
    Ok(FetchStats {
        wire_bytes: wire.load(Ordering::Relaxed) as u64,
        raw_bytes: raw.load(Ordering::Relaxed) as u64,
        fetched: fetched.load(Ordering::Relaxed) as u64,
        requests: requests.load(Ordering::Relaxed) as u64,
        useful_bytes: missing.iter().map(|g| g.useful).sum(),
        selective_retries: selective.load(Ordering::Relaxed) as u64,
        throttle_waits: throttled.load(Ordering::Relaxed) as u64,
        concurrency_mode: auto as u64,
        concurrency_peak: match &adaptive {
            Some((_, gate)) => gate.peak(),
            None => workers as u64,
        },
        aimd_decreases: adaptive.as_ref().map_or(0, |(c, _)| c.decreases()),
        ..FetchStats::default()
    })
}

/// Transparently retry a transient range failure once: transport errors and
/// short reads get a single second attempt before they become fatal.
///
/// Every failed attempt — even one the retry then papers over — is reported
/// to the AIMD controller (when adaptive mode is on): a remote that starts
/// erroring or truncating under load must slow us down *before* the errors
/// become fatal, not after.
fn get_range_retrying(
    source: &dyn RangeSource,
    rel: &str,
    offset: u64,
    len: u64,
    requests: &mut usize,
    ctrl: Option<&AimdController>,
) -> Result<Vec<u8>> {
    for attempt in 0..2 {
        *requests += 1;
        match source.get_range(rel, offset, len) {
            Ok(bytes) if bytes.len() as u64 >= len => return Ok(bytes),
            Ok(bytes) if attempt == 1 => bail!(
                "CAVS-E-RANGE-LENGTH-MISMATCH: {rel} returned {} of {len} bytes at {offset}",
                bytes.len()
            ),
            Err(e) if attempt == 1 => {
                return Err(e.context(format!("CAVS-E-RANGE-TRANSFER-FAILED: {rel} at {offset}")))
            }
            _ => {
                // Transient failure (transport error or short read) that a
                // retry will absorb: still a pressure signal.
                if let Some(c) = ctrl {
                    c.on_pressure();
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    unreachable!()
}

/// Fetch one coalesced range and land every chunk it covers: slice each
/// chunk out of the response, decode, BLAKE3-verify and cache it. The
/// coalescing never weakens verification — every chunk is still checked
/// against its own hash.
///
/// Failure recovery is *selective*: a chunk that fails verification inside
/// an otherwise healthy range is re-requested alone (its exact subrange, a
/// fresh request that skips whatever stale or truncated body the group GET
/// got) instead of repeating the whole range; only if the chunk fails again
/// does the fetch abort.
fn fetch_group(
    source: &dyn RangeSource,
    group: &RangeGroup,
    cache: &ChunkCache,
    ctrl: Option<&AimdController>,
) -> Result<GroupOutcome> {
    let mut out = GroupOutcome::default();
    let wire = get_range_retrying(
        source,
        &group.pack,
        group.start,
        group.span,
        &mut out.requests,
        ctrl,
    )?;
    out.wire = wire.len();
    for entry in &group.chunks {
        let hash: ChunkHash = from_hex(&entry.hash)
            .with_context(|| format!("bad hash {} in chunk-map", entry.hash))?;
        let at = (entry.pack_offset_abs - group.start) as usize;
        let raw = match decode_chunk(&wire[at..at + entry.len_stored as usize], entry, &hash) {
            Ok(raw) => raw,
            Err(_) => {
                // Selective retry: this chunk's exact bytes, fresh request.
                out.selective_retries += 1;
                let alone = get_range_retrying(
                    source,
                    &group.pack,
                    entry.pack_offset_abs,
                    entry.len_stored as u64,
                    &mut out.requests,
                    ctrl,
                )?;
                out.wire += alone.len();
                decode_chunk(&alone[..entry.len_stored as usize], entry, &hash).map_err(|e| {
                    e.context(format!(
                        "chunk {} failed verification twice (pack {} may be corrupt or stale)",
                        entry.hash, group.pack
                    ))
                })?
            }
        };
        out.raw += raw.len();
        cache.put(&hash, &raw)?;
    }
    out.chunks = group.chunks.len();
    Ok(out)
}

/// Decode one chunk's stored bytes (zstd, BG4) and verify it against its
/// BLAKE3 identity. Returns the raw bytes only when everything matches.
fn decode_chunk(stored: &[u8], entry: &ChunkMapEntry, hash: &ChunkHash) -> Result<Vec<u8>> {
    let mut raw = if entry.flags & CHUNK_FLAG_ZSTD != 0 {
        zstd::bulk::decompress(stored, entry.len_raw as usize)
            .map_err(|e| anyhow::anyhow!("decompressing chunk {}: {e}", entry.hash))?
    } else {
        stored.to_vec()
    };
    if entry.flags & CHUNK_FLAG_BG4 != 0 {
        raw = bg4_ungroup(&raw);
    }
    if raw.len() != entry.len_raw as usize || hash_chunk(&raw) != *hash {
        bail!(
            "CAVS-E-CHUNK-HASH-MISMATCH: chunk {} failed verification",
            entry.hash
        );
    }
    Ok(raw)
}

/// Every unique chunk hash the manifest references (init + segment chunks).
fn manifest_chunk_hashes(manifest: &cavs_proto::Manifest) -> Vec<String> {
    let mut set = std::collections::HashSet::new();
    for t in &manifest.tracks {
        for c in &t.init_chunks {
            set.insert(c.hash.clone());
        }
    }
    for s in &manifest.segments {
        for c in &s.chunks {
            set.insert(c.hash.clone());
        }
    }
    set.into_iter().collect()
}

/// Enforce the manifest's Ed25519 content signature against a trusted key.
fn verify_signature(manifest: &cavs_proto::Manifest, trusted_hex: &str) -> Result<()> {
    use ed25519_dalek::Verifier;
    let sig_hex = manifest
        .signature
        .as_deref()
        .context("asset is not signed but a pubkey was given")?;
    let signer_hex = manifest
        .signer_pubkey
        .as_deref()
        .context("asset signature has no public key")?;
    if !signer_hex.eq_ignore_ascii_case(trusted_hex) {
        bail!("asset is signed by an untrusted key {signer_hex}");
    }
    let leaves: Vec<ChunkHash> = manifest
        .chunk_table
        .iter()
        .map(|h| from_hex(h).context("bad hash in chunk_table"))
        .collect::<Result<_>>()?;
    let root = cavs_hash::merkle_root(&leaves);
    if !manifest
        .merkle_root
        .eq_ignore_ascii_case(&cavs_hash::to_hex(&root))
    {
        bail!("manifest merkle_root does not match its chunk_table");
    }
    let pk: [u8; 32] = decode_hex(signer_hex, 32)?.try_into().unwrap();
    let sig: [u8; 64] = decode_hex(sig_hex, 64)?.try_into().unwrap();
    let key = ed25519_dalek::VerifyingKey::from_bytes(&pk).context("invalid signer key")?;
    let message = cavs_hash::content_signature_message(&root, leaves.len() as u64);
    key.verify(&message, &ed25519_dalek::Signature::from_bytes(&sig))
        .map_err(|_| anyhow::anyhow!("content signature is INVALID"))?;
    // Every referenced chunk must be covered by the signed table.
    let table: std::collections::HashSet<&str> =
        manifest.chunk_table.iter().map(|s| s.as_str()).collect();
    for h in manifest_chunk_hashes(manifest) {
        if !table.contains(h.as_str()) {
            bail!("chunk {h} referenced but not covered by the signed table");
        }
    }
    Ok(())
}

fn decode_hex(s: &str, len: usize) -> Result<Vec<u8>> {
    if s.len() != len * 2 {
        bail!("expected {} hex chars, got {}", len * 2, s.len());
    }
    (0..len)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).context("bad hex"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pack: &str, offset: u64, len: u32) -> ChunkMapEntry {
        ChunkMapEntry {
            hash: format!("{pack}-{offset}"),
            len_raw: len,
            len_stored: len,
            flags: 0,
            pack: pack.to_string(),
            pack_offset_abs: offset,
        }
    }

    #[test]
    fn adjacent_chunks_coalesce_into_one_range() {
        // A 64 KiB gap is tolerated when the chunks around it are big
        // enough that the waste stays under the amplification cap.
        let big = 1_000_000u32;
        let groups = plan_range_groups(vec![
            entry("p", 100, big),
            entry("p", 100 + big as u64, big),
            entry("p", 100 + 2 * big as u64 + MAX_COALESCE_GAP, big),
        ]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].start, 100);
        assert_eq!(groups[0].span, 3 * big as u64 + MAX_COALESCE_GAP);
        assert_eq!(groups[0].useful, 3 * big as u64);
        assert_eq!(groups[0].chunks.len(), 3);
    }

    #[test]
    fn gap_pack_and_span_limits_split_groups() {
        let groups = plan_range_groups(vec![
            entry("p", 0, 10),
            entry("p", 10 + MAX_COALESCE_GAP + 1, 10), // gap too large
            entry("q", 0, 10),                         // different pack
        ]);
        assert_eq!(groups.len(), 3);

        // Span cap: two large chunks that would exceed the max stay apart.
        let half = (MAX_COALESCED_RANGE / 2 + 1) as u32;
        let groups = plan_range_groups(vec![entry("p", 0, half), entry("p", half as u64, half)]);
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn waste_ratio_cap_bounds_read_amplification() {
        // Tiny chunks separated by tolerable gaps: without the waste cap
        // they'd coalesce into one range that is ~99% gap (65× the useful
        // bytes). The cap forces them apart.
        let groups = plan_range_groups(vec![
            entry("p", 0, 1000),
            entry("p", 1000 + MAX_COALESCE_GAP, 1000),
            entry("p", 2 * (1000 + MAX_COALESCE_GAP), 1000),
        ]);
        assert_eq!(groups.len(), 3, "sparse tiny chunks must not coalesce");
        for g in &groups {
            assert!((g.span - g.useful) * 100 <= g.useful * MAX_WASTE_RATIO_PCT);
        }

        // Amplification stays bounded even in mixed runs: every planned
        // group respects the cap by construction.
        let mixed: Vec<ChunkMapEntry> = (0..50)
            .map(|i| entry("p", i * 40_000, if i % 7 == 0 { 30_000 } else { 500 }))
            .collect();
        for g in plan_range_groups(mixed) {
            assert!((g.span - g.useful) * 100 <= g.useful * MAX_WASTE_RATIO_PCT);
        }
    }

    #[test]
    fn unsorted_input_is_sorted_before_grouping() {
        let groups = plan_range_groups(vec![entry("p", 60, 40), entry("p", 0, 60)]);
        assert_eq!(groups.len(), 1);
        assert_eq!((groups[0].start, groups[0].span), (0, 100));
    }

    #[test]
    fn byte_budget_blocks_until_released_and_clamps_oversize() {
        let budget = ByteBudget {
            max: 100,
            used: Mutex::new(0),
            freed: std::sync::Condvar::new(),
        };
        // Larger than the whole budget: clamped, not deadlocked.
        let (permit, waited) = budget.acquire(1000);
        assert!(!waited);
        drop(permit);

        let (first, _) = budget.acquire(80);
        std::thread::scope(|s| {
            let h = s.spawn(|| {
                let (_p, waited) = budget.acquire(50); // must wait for `first`
                waited
            });
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(first);
            assert!(h.join().unwrap(), "second acquire had to wait");
        });
    }

    /// A source over one in-memory pack that serves a corrupted body for
    /// multi-chunk (group) ranges but clean bytes for single-chunk
    /// re-requests — the stale-CDN-range shape selective retry exists for.
    struct FlakySource {
        pack: Vec<u8>,
        corrupt_group_reads: bool,
        corrupt_all_reads: bool,
        short_reads_left: Mutex<u32>,
        single_len: u64,
    }

    impl RangeSource for FlakySource {
        fn get_range(&self, _rel: &str, offset: u64, len: u64) -> Result<Vec<u8>> {
            {
                let mut left = self.short_reads_left.lock().unwrap();
                if *left > 0 {
                    *left -= 1;
                    return Ok(self.pack[offset as usize..(offset + len - 1) as usize].to_vec());
                }
            }
            let mut out = self.pack[offset as usize..(offset + len) as usize].to_vec();
            if self.corrupt_all_reads || (self.corrupt_group_reads && len > self.single_len) {
                out[0] ^= 0xff;
            }
            Ok(out)
        }
    }

    fn two_chunk_group() -> (Vec<u8>, RangeGroup) {
        let a = vec![0xaau8; 500];
        let b = vec![0xbbu8; 500];
        let mut pack = a.clone();
        pack.extend_from_slice(&b);
        let mk = |data: &[u8], offset: u64| ChunkMapEntry {
            hash: cavs_hash::to_hex(&hash_chunk(data)),
            len_raw: data.len() as u32,
            len_stored: data.len() as u32,
            flags: 0,
            pack: "pk".into(),
            pack_offset_abs: offset,
        };
        let chunks = vec![mk(&a, 0), mk(&b, 500)];
        let group = RangeGroup {
            pack: "pk".into(),
            start: 0,
            span: 1000,
            useful: 1000,
            chunks,
        };
        (pack, group)
    }

    #[test]
    fn corrupt_chunk_in_range_is_refetched_alone() {
        let (pack, group) = two_chunk_group();
        let dir = tempfile::tempdir().unwrap();
        let cache = ChunkCache::open(dir.path()).unwrap();
        let source = FlakySource {
            pack,
            corrupt_group_reads: true,
            corrupt_all_reads: false,
            short_reads_left: Mutex::new(0),
            single_len: 500,
        };
        let out = fetch_group(&source, &group, &cache, None).unwrap();
        assert_eq!(out.chunks, 2);
        assert_eq!(out.selective_retries, 1, "only the bad chunk re-fetched");
        assert_eq!(out.requests, 2, "one group GET + one selective GET");
    }

    #[test]
    fn persistent_corruption_fails_with_diagnosis_after_selective_retry() {
        let (pack, group) = two_chunk_group();
        let dir = tempfile::tempdir().unwrap();
        let cache = ChunkCache::open(dir.path()).unwrap();
        let source = FlakySource {
            pack,
            corrupt_group_reads: false,
            corrupt_all_reads: true,
            short_reads_left: Mutex::new(0),
            single_len: 500,
        };
        let err = format!(
            "{:#}",
            fetch_group(&source, &group, &cache, None).unwrap_err()
        );
        assert!(err.contains("twice"), "got: {err}");
        assert!(err.contains("CAVS-E-CHUNK-HASH-MISMATCH"), "got: {err}");
    }

    #[test]
    fn short_range_read_gets_one_retry_then_fails() {
        let (pack, group) = two_chunk_group();
        let dir = tempfile::tempdir().unwrap();
        let cache = ChunkCache::open(dir.path()).unwrap();
        // One short read: the transparent retry succeeds.
        let source = FlakySource {
            pack: pack.clone(),
            corrupt_group_reads: false,
            corrupt_all_reads: false,
            short_reads_left: Mutex::new(1),
            single_len: 500,
        };
        let out = fetch_group(&source, &group, &cache, None).unwrap();
        assert_eq!((out.chunks, out.requests), (2, 2));

        // Persistent truncation: a stable error, not a hang or a panic.
        let source = FlakySource {
            pack,
            corrupt_group_reads: false,
            corrupt_all_reads: false,
            short_reads_left: Mutex::new(9),
            single_len: 500,
        };
        let dir2 = tempfile::tempdir().unwrap();
        let cache2 = ChunkCache::open(dir2.path()).unwrap();
        let err = format!(
            "{:#}",
            fetch_group(&source, &group, &cache2, None).unwrap_err()
        );
        assert!(err.contains("CAVS-E-RANGE-LENGTH-MISMATCH"), "got: {err}");
    }

    #[test]
    fn env_override_beats_options_and_tolerates_junk() {
        assert_eq!(concurrency_override(None, 8), 8, "no env: caller wins");
        assert_eq!(concurrency_override(Some("auto"), 8), 0);
        assert_eq!(concurrency_override(Some(" AUTO "), 8), 0);
        assert_eq!(concurrency_override(Some("16"), 8), 16);
        assert_eq!(concurrency_override(Some("0"), 8), 0);
        assert_eq!(concurrency_override(Some("many"), 8), 8, "junk: fallback");
    }

    /// N single-chunk groups over one in-memory pack, mirroring what
    /// [`plan_range_groups`] produces for a sparse update.
    fn in_memory_groups(n: usize, chunk_len: usize) -> (Vec<u8>, Vec<RangeGroup>) {
        let mut pack = Vec::new();
        let mut groups = Vec::new();
        for i in 0..n {
            let data = vec![i as u8; chunk_len];
            let offset = pack.len() as u64;
            pack.extend_from_slice(&data);
            let entry = ChunkMapEntry {
                hash: cavs_hash::to_hex(&hash_chunk(&data)),
                len_raw: chunk_len as u32,
                len_stored: chunk_len as u32,
                flags: 0,
                pack: "pk".into(),
                pack_offset_abs: offset,
            };
            groups.push(RangeGroup {
                pack: "pk".into(),
                start: offset,
                span: chunk_len as u64,
                useful: chunk_len as u64,
                chunks: vec![entry],
            });
        }
        (pack, groups)
    }

    #[test]
    fn auto_mode_fetches_everything_and_reports_adaptive_stats() {
        let n = 12;
        let (pack, groups) = in_memory_groups(n, 100);
        let source = FlakySource {
            pack,
            corrupt_group_reads: false,
            corrupt_all_reads: false,
            short_reads_left: Mutex::new(0),
            single_len: 0,
        };
        let dir = tempfile::tempdir().unwrap();
        let cache = ChunkCache::open(dir.path()).unwrap();
        let opts = FetchOptions {
            connections: 0, // AUTO
            ..FetchOptions::default()
        };
        let total: u64 = groups.iter().map(|g| g.span).sum();
        let stats = fetch_missing_parallel(&source, &groups, &cache, &opts, total).unwrap();
        assert_eq!(stats.fetched, n as u64);
        assert_eq!(stats.wire_bytes, total);
        assert_eq!(stats.concurrency_mode, 1, "AUTO must be reported");
        assert!(stats.concurrency_peak >= 1);
        assert_eq!(stats.aimd_decreases, 0, "clean source: no pressure");
        for g in &groups {
            assert!(cache.contains(&g.chunks[0].hash), "every chunk cached");
        }
    }

    #[test]
    fn auto_mode_counts_a_decrease_under_pressure() {
        // One short read: the transparent retry absorbs it, the fetch
        // still succeeds, but the AIMD controller must have backed off.
        let n = 8;
        let (pack, groups) = in_memory_groups(n, 100);
        let source = FlakySource {
            pack,
            corrupt_group_reads: false,
            corrupt_all_reads: false,
            short_reads_left: Mutex::new(1),
            single_len: 0,
        };
        let dir = tempfile::tempdir().unwrap();
        let cache = ChunkCache::open(dir.path()).unwrap();
        let opts = FetchOptions {
            connections: 0,
            ..FetchOptions::default()
        };
        let total: u64 = groups.iter().map(|g| g.span).sum();
        let stats = fetch_missing_parallel(&source, &groups, &cache, &opts, total).unwrap();
        assert_eq!(stats.fetched, n as u64);
        assert_eq!(stats.concurrency_mode, 1);
        assert_eq!(stats.aimd_decreases, 1, "short reads are pressure");
    }

    #[test]
    fn fixed_mode_reports_fixed_stats() {
        let n = 4;
        let (pack, groups) = in_memory_groups(n, 100);
        let source = FlakySource {
            pack,
            corrupt_group_reads: false,
            corrupt_all_reads: false,
            short_reads_left: Mutex::new(0),
            single_len: 0,
        };
        let dir = tempfile::tempdir().unwrap();
        let cache = ChunkCache::open(dir.path()).unwrap();
        let opts = FetchOptions {
            connections: 2,
            ..FetchOptions::default()
        };
        let total: u64 = groups.iter().map(|g| g.span).sum();
        let stats = fetch_missing_parallel(&source, &groups, &cache, &opts, total).unwrap();
        assert_eq!(stats.fetched, n as u64);
        assert_eq!(stats.concurrency_mode, 0);
        assert_eq!(stats.concurrency_peak, 2, "the fixed pool size");
        assert_eq!(stats.aimd_decreases, 0);
    }
}
