//! Reconstruct output files from a complete cache, streaming to disk with a
//! `.part` → verify → atomic rename so a crash never promotes a torn file.
//! Container payloads only (builds / directory trees); media tracks are
//! served by the full `cavs-client`, not this embeddable engine.

use crate::cache::ChunkCache;
use anyhow::{bail, Context, Result};
use cavs_hash::from_hex;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Total logical (reconstructed) size across all tracks.
pub fn logical_bytes(manifest: &cavs_proto::Manifest) -> u64 {
    let mut total = 0u64;
    for t in &manifest.tracks {
        for c in &t.init_chunks {
            total += c.len as u64;
        }
    }
    for s in &manifest.segments {
        total += s.chunks.iter().map(|c| c.len as u64).sum::<u64>();
    }
    total
}

/// Write every track's file under `output`, concatenating its chunks in
/// order from the cache. Verifies each file against its `sha256:<name>` meta
/// digest when present.
pub fn reconstruct(
    manifest: &cavs_proto::Manifest,
    cache: &ChunkCache,
    output: &Path,
) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(output)?;
    let sha_by_name: std::collections::HashMap<&str, &str> = manifest
        .meta
        .iter()
        .filter_map(|(k, v)| k.strip_prefix("sha256:").map(|n| (n, v.as_str())))
        .collect();

    let mut primaries = Vec::new();
    for track in &manifest.tracks {
        if track.kind == "video" || track.kind == "audio" {
            bail!("media tracks are not supported by the embeddable fetch engine");
        }
        if track.name.contains("..") || track.name.starts_with('/') {
            bail!("unsafe track name: {}", track.name);
        }
        let mut segs: Vec<_> = manifest
            .segments
            .iter()
            .filter(|s| s.track_id == track.track_id)
            .collect();
        segs.sort_by_key(|s| (s.pts_start, s.segment_id));

        let final_path = output.join(&track.name);
        if let Some(parent) = final_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = final_path.with_extension("cavspart");
        let mut file = std::io::BufWriter::new(
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?,
        );
        let mut hasher = Sha256::new();
        let write_chunk = |hex: &str,
                           file: &mut std::io::BufWriter<std::fs::File>,
                           hasher: &mut Sha256|
         -> Result<()> {
            use std::io::Write as _;
            let hash = from_hex(hex).with_context(|| format!("bad chunk hash {hex}"))?;
            let bytes = cache
                .get(&hash)?
                .with_context(|| format!("chunk {hex} missing from cache during reconstruct"))?;
            hasher.update(&bytes);
            file.write_all(&bytes)?;
            Ok(())
        };

        for c in &track.init_chunks {
            write_chunk(&c.hash, &mut file, &mut hasher)?;
        }
        for seg in &segs {
            for c in &seg.chunks {
                write_chunk(&c.hash, &mut file, &mut hasher)?;
            }
        }
        use std::io::Write as _;
        file.flush()?;
        drop(file);

        if let Some(expected) = sha_by_name.get(track.name.as_str()) {
            let got = hex_lower(&hasher.finalize());
            if !got.eq_ignore_ascii_case(expected) {
                let _ = std::fs::remove_file(&tmp);
                bail!("reconstructed {} failed SHA-256 verification", track.name);
            }
        }
        std::fs::rename(&tmp, &final_path)
            .with_context(|| format!("promoting {}", final_path.display()))?;
        if track.codec == "raw" {
            primaries.push(final_path);
        }
    }
    Ok(primaries)
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
