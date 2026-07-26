//! Content-addressed parallel chunk download (v1.4.0).
//!
//! The session/batch path is stateful — the origin mutates a per-session
//! have-set to inline each cold chunk exactly once — so it is inherently
//! sequential (one batch round-trip at a time). For container payloads
//! (raw builds, directory trees: the generic file focus) the client already
//! holds the manifest and its own cache, so it can compute the missing set
//! itself and fetch those immutable chunks **by hash, concurrently**, from
//! the edge-cacheable `/api/assets/{asset}/chunks/{hash}` endpoint.
//!
//! This trades the server's session dedup (irrelevant once the client dedups
//! the hash list itself) for N connections in flight, and — because every
//! chunk is content-addressed and idempotent — needs no ordering and no
//! bloom false-positive repair. The same by-hash primitive backs the
//! serverless/CDN fetch mode.

use crate::cache::ChunkCache;
use crate::retry;
use anyhow::{bail, Result};
use cavs_hash::{hash_chunk, to_hex, ChunkHash};
use cavs_proto::errors::ErrorCode;
use std::io::Read as _;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

/// Request header asking the origin for the chunk exactly as stored
/// (possibly zstd) plus wire metadata; an origin that predates it simply
/// serves raw bytes, which this path detects and handles.
pub const ACCEPT_STORED_HEADER: &str = "x-cavs-accept-stored";
pub const COMPRESSION_HEADER: &str = "x-cavs-compression";
pub const RAW_LEN_HEADER: &str = "x-cavs-raw-len";

/// Default concurrent connections when the caller does not override it.
pub const DEFAULT_CONNECTIONS: usize = 8;

#[derive(Debug, Default, Clone, Copy)]
pub struct ParallelStats {
    /// Bytes actually pulled over the wire (stored, possibly compressed).
    pub wire_bytes: u64,
    /// Decompressed bytes written to the cache.
    pub raw_bytes: u64,
    /// Chunks fetched.
    pub count: u64,
}

/// Fetch every hash in `hashes` (assumed unique and absent locally) into
/// `cache`, using up to `connections` worker threads. Each chunk is
/// verified by BLAKE3 before it is cached, so a corrupt or wrong-hash
/// response fails the whole fetch rather than poisoning the cache. Returns
/// aggregate egress stats.
pub fn fetch_chunks_parallel(
    agent: &ureq::Agent,
    server: &str,
    asset: &str,
    hashes: &[ChunkHash],
    cache: &ChunkCache,
    connections: usize,
) -> Result<ParallelStats> {
    if hashes.is_empty() {
        return Ok(ParallelStats::default());
    }
    let workers = connections.max(1).min(hashes.len());
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let first_error: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    let wire = AtomicUsize::new(0);
    let raw = AtomicUsize::new(0);
    let count = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for _ in 0..workers {
            // A cloned Agent shares the connection pool but lets each worker
            // hold its own in-flight connection.
            let agent = agent.clone();
            scope.spawn(|| {
                let agent = agent;
                loop {
                    if failed.load(Ordering::Relaxed) {
                        return;
                    }
                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    if idx >= hashes.len() {
                        return;
                    }
                    let hash = hashes[idx];
                    match fetch_one(&agent, server, asset, &hash) {
                        Ok((raw_bytes, wire_len)) => {
                            if let Err(e) = cache.put(&hash, &raw_bytes) {
                                record_error(&failed, &first_error, e);
                                return;
                            }
                            wire.fetch_add(wire_len, Ordering::Relaxed);
                            raw.fetch_add(raw_bytes.len(), Ordering::Relaxed);
                            count.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            record_error(&failed, &first_error, e);
                            return;
                        }
                    }
                }
            });
        }
    });

    if let Some(e) = first_error.into_inner().unwrap() {
        return Err(e);
    }
    Ok(ParallelStats {
        wire_bytes: wire.load(Ordering::Relaxed) as u64,
        raw_bytes: raw.load(Ordering::Relaxed) as u64,
        count: count.load(Ordering::Relaxed) as u64,
    })
}

fn record_error(failed: &AtomicBool, slot: &Mutex<Option<anyhow::Error>>, e: anyhow::Error) {
    failed.store(true, Ordering::Relaxed);
    let mut guard = slot.lock().unwrap();
    if guard.is_none() {
        *guard = Some(e);
    }
}

/// GET one chunk by hash, asking for stored bytes; decompress if the origin
/// tagged them zstd, verify, and return `(raw_bytes, wire_len)`.
fn fetch_one(
    agent: &ureq::Agent,
    server: &str,
    asset: &str,
    hash: &ChunkHash,
) -> Result<(Vec<u8>, usize)> {
    let hex = to_hex(hash);
    let url = format!("{server}/api/assets/{asset}/chunks/{hex}");
    let resp = retry::with_retry(&format!("GET {url}"), || {
        agent.get(&url).set(ACCEPT_STORED_HEADER, "1").call()
    })?;
    let compression: u8 = resp
        .header(COMPRESSION_HEADER)
        .and_then(|s| s.parse().ok())
        .unwrap_or(cavs_proto::WIRE_COMPRESSION_NONE);
    let len_raw: Option<usize> = resp.header(RAW_LEN_HEADER).and_then(|s| s.parse().ok());

    let mut wire = Vec::new();
    resp.into_reader().read_to_end(&mut wire)?;
    let wire_len = wire.len();

    let raw = match compression {
        cavs_proto::WIRE_COMPRESSION_NONE => wire,
        cavs_proto::WIRE_COMPRESSION_ZSTD => {
            // Trust the raw length hint when present; otherwise decode with a
            // generous cap (a single chunk is bounded by the max chunk size).
            let cap = len_raw.unwrap_or(16 * 1024 * 1024);
            zstd::bulk::decompress(&wire, cap)
                .map_err(|e| anyhow::anyhow!("decompressing chunk {hex}: {e}"))?
        }
        other => bail!("unknown wire compression {other} for chunk {hex}"),
    };
    if let Some(n) = len_raw {
        if raw.len() != n {
            bail!(
                "{}",
                ErrorCode::ChunkHashMismatch.msg(format!(
                    "chunk {hex}: raw length {} != declared {n}",
                    raw.len()
                ))
            );
        }
    }
    if hash_chunk(&raw) != *hash {
        bail!(
            "{}",
            ErrorCode::ChunkHashMismatch.msg(format!("chunk {hex} failed hash verification"))
        );
    }
    Ok((raw, wire_len))
}
