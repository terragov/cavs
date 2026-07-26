//! Packing (conversion into .cavs).
//!
//! Video mode: each input is segmented with ffmpeg into CMAF/fMP4
//! (init.mp4 + seg_%05d.m4s + media.m3u8), and those artifacts are chunked,
//! deduplicated and stored. Byte-identical content across segments and across
//! inputs (shared intros, repeated stills, same episode at two cuts...) is
//! stored exactly once.
//!
//! Raw mode: arbitrary files are chunked with FastCDC and stored as data
//! tracks; unpacking reproduces them byte-for-byte.

use crate::profile::{self, ChunkProfile, CostWeights};
use crate::{classify, ffmpeg, report, ChunkModeArg};
use anyhow::{bail, Context, Result};
use cavs_chunker::ChunkMode;
use cavs_format::{SegmentRecord, TrackKind, TrackRecord, Writer, SEGMENT_FLAG_RANDOM_ACCESS};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Track-id namespace: playlist/data companion tracks of video track `n`
/// live at `PLAYLIST_TRACK_BASE + n`.
pub const PLAYLIST_TRACK_BASE: u32 = 1000;

pub struct PackOptions {
    pub segment_time: f64,
    pub mode: Option<ChunkModeArg>,
    pub chunk_size: Option<usize>,
    /// `auto`, or a fixed profile label (see [`ChunkProfile`]). Wins over
    /// `mode`/`chunk_size` when set.
    pub profile: Option<String>,
    /// Previous version of the (single) input for update-aware auto profile.
    pub prev: Option<PathBuf>,
    /// Also emit `<output>.bootstrap.zst` (raw mode, single input).
    pub bootstrap: bool,
    pub compress: bool,
    pub zstd_level: i32,
    pub force_transcode: bool,
    pub sign_key: Option<PathBuf>,
    /// Report how much of the new payload a client holding the signed old
    /// version could reuse via hybrid reconstruction (v0.6.0).
    pub against_signature: Option<PathBuf>,
}

/// Load an Ed25519 secret key (hex file from `cavs keygen`) and attach it.
fn apply_signing(w: &mut Writer, opts: &PackOptions) -> Result<()> {
    let Some(path) = &opts.sign_key else {
        return Ok(());
    };
    sign_writer(w, path)
}

/// Attach a signing key to any writer (shared with `pack-dir`).
pub fn sign_writer(w: &mut Writer, path: &Path) -> Result<()> {
    let hex = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read signing key {}", path.display()))?;
    let hex = hex.trim();
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("{} is not a 64-hex-char Ed25519 secret key", path.display());
    }
    let mut secret = [0u8; 32];
    for (i, byte) in secret.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
    }
    w.sign_with(&secret);
    eprintln!("[pack] content will be signed (Ed25519)");
    Ok(())
}

impl PackOptions {
    fn media_mode(&self) -> ChunkMode {
        match self.mode {
            Some(m) => m.to_mode(self.chunk_size),
            None => ChunkMode::media_default(),
        }
    }

    fn asset_mode(&self) -> ChunkMode {
        match self.mode {
            Some(m) => m.to_mode(self.chunk_size),
            None => ChunkMode::asset_default(),
        }
    }
}

/// Resolve the chunk mode for one raw input. With `--profile auto` the
/// payload is classified and the recommended candidate profiles are measured
/// on the real bytes (against `--prev` when given); a forced profile label
/// maps directly; otherwise the legacy `--mode`/default applies.
/// Returns the mode plus the profile label to record in metadata.
fn resolve_raw_mode(
    input: &Path,
    data: &[u8],
    opts: &PackOptions,
) -> Result<(ChunkMode, Option<&'static str>, Option<&'static str>)> {
    let Some(profile_arg) = opts.profile.as_deref() else {
        return Ok((opts.asset_mode(), None, None));
    };
    if profile_arg != "auto" {
        let p = ChunkProfile::parse(profile_arg)?;
        return Ok((p.to_mode(), Some(p.label()), None));
    }

    let payload = classify::classify(input, data);
    let prev = match &opts.prev {
        Some(p) => Some(profile::load_prev(p)?),
        None => None,
    };

    // Update-heavy payload, first version, bootstrap covers the cold path:
    // there is nothing to measure updates against yet, so pick the
    // benchmark-validated update profile instead of letting the cold-egress
    // sweep lock the whole version stream into large chunks. Subsequent
    // versions (--prev) measure real reuse and keep continuity.
    if payload.likely_update_heavy && prev.is_none() && opts.bootstrap {
        // fastcdc-16k: on the real build pairs it cut update egress 60–68%
        // vs fastcdc-64k for ~+4% cold chunk-path egress — and this branch
        // only fires when --bootstrap serves the cold path anyway. The
        // profile label locks the stream's boundaries; later versions keep
        // continuity through --prev.
        let p = ChunkProfile::FastCdc16K;
        eprintln!(
            "[pack] {} classified as {} -> profile {} (update-heavy; cold path served by bootstrap)",
            input.display(),
            payload.kind.label(),
            p.label(),
        );
        return Ok((p.to_mode(), Some(p.label()), Some(payload.kind.label())));
    }

    let weights = if prev.is_some() {
        CostWeights::live_updates()
    } else {
        CostWeights::cold_install()
    };
    let estimates: Vec<_> = payload
        .recommended_profiles
        .iter()
        .map(|&p| profile::estimate(data, prev.as_ref(), p, opts.zstd_level))
        .collect();
    let best = profile::choose_best(&estimates, &weights);
    eprintln!(
        "[pack] {} classified as {} (entropy {:.2}, zstd probe {:.3}) -> profile {}",
        input.display(),
        payload.kind.label(),
        payload.entropy_score,
        payload.zstd_sample_ratio,
        best.label(),
    );
    Ok((
        best.to_mode(),
        Some(best.label()),
        Some(payload.kind.label()),
    ))
}

/// Write the full bootstrap artifact next to the `.cavs`: the whole input
/// zstd-compressed at a high level, so a cache-less client can install at
/// full-artifact cost and seed its chunk cache locally (P0-1 dual route).
/// Level 19 spends pack-time CPU once to minimise every cold install.
const BOOTSTRAP_ZSTD_LEVEL: i32 = 19;

fn write_bootstrap(output: &Path, name: &str, data: &[u8], w: &mut Writer) -> Result<()> {
    let path = PathBuf::from(format!("{}.bootstrap.zst", output.display()));
    let compressed = zstd::bulk::compress(data, BOOTSTRAP_ZSTD_LEVEL)
        .context("compressing bootstrap artifact")?;
    std::fs::write(&path, &compressed)
        .with_context(|| format!("cannot write {}", path.display()))?;
    let blake3_hex = cavs_hash::to_hex(&cavs_hash::hash_chunk(&compressed));
    w.set_meta("bootstrap.name", name);
    w.set_meta("bootstrap.size", &compressed.len().to_string());
    w.set_meta("bootstrap.blake3", &blake3_hex);
    eprintln!(
        "[pack] bootstrap artifact: {} ({} bytes, zstd-{BOOTSTRAP_ZSTD_LEVEL})",
        path.display(),
        compressed.len()
    );
    Ok(())
}

/// Add `data` as chunks and return the chunk indices. Hash + compression
/// run on all cores; the output file is byte-identical to the serial path.
fn add_chunked(w: &mut Writer, data: &[u8], mode: ChunkMode) -> Result<Vec<u32>> {
    let ranges = cavs_chunker::split(data, mode);
    Ok(w.add_chunks_parallel(data, &ranges)?)
}

/// Derive a unique, filesystem-safe logical name from an input path.
fn unique_stem(path: &Path, used: &mut HashSet<String>) -> String {
    let base = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "input".to_string())
        .replace(['/', '\\'], "_");
    let mut name = base.clone();
    let mut n = 1;
    while !used.insert(name.clone()) {
        n += 1;
        name = format!("{base}-{n}");
    }
    name
}

pub fn pack_video(inputs: &[PathBuf], output: &Path, opts: &PackOptions) -> Result<()> {
    ffmpeg::require("ffmpeg")?;

    let uuid = *uuid::Uuid::new_v4().as_bytes();
    let mut w = Writer::create(output, uuid, 1000, opts.compress)
        .with_context(|| format!("cannot create {}", output.display()))?;
    w.set_zstd_level(opts.zstd_level);
    apply_signing(&mut w, opts)?;
    w.set_meta("packer", concat!("cavs-cli ", env!("CARGO_PKG_VERSION")));
    w.set_meta("payload", "cmaf-hls");
    w.set_meta("segment_time", &format!("{}", opts.segment_time));

    let mut used_names = HashSet::new();
    let mut segment_id = 0u64;

    for (i, input) in inputs.iter().enumerate() {
        let track_id = i as u32 + 1;
        let stem = unique_stem(input, &mut used_names);
        eprintln!("[pack] segmenting {} with ffmpeg...", input.display());

        let workdir = tempfile::tempdir().context("cannot create temp dir")?;
        let copied = ffmpeg::segment_to_cmaf(
            input,
            workdir.path(),
            opts.segment_time,
            opts.force_transcode,
        )?;
        eprintln!(
            "[pack]   mode: {}",
            if copied {
                "stream copy"
            } else {
                "transcode h264+aac"
            }
        );

        // Init segment: pinned into the global dictionary (bootstrap payload).
        let init_bytes = std::fs::read(workdir.path().join("init.mp4"))
            .context("ffmpeg did not produce init.mp4")?;
        let init_chunks = add_chunked(&mut w, &init_bytes, opts.media_mode())?;
        for &c in &init_chunks {
            w.pin_dict(c)?;
        }

        let playlist_text = std::fs::read_to_string(workdir.path().join("media.m3u8"))
            .context("ffmpeg did not produce media.m3u8")?;
        let playlist_segments = ffmpeg::parse_playlist(&playlist_text);
        if playlist_segments.is_empty() {
            bail!("playlist has no media segments");
        }

        w.add_track(TrackRecord {
            track_id,
            kind: TrackKind::Video,
            flags: 0,
            codec: ffmpeg::probe_codecs(input),
            name: stem.clone(),
            timescale: 1000,
            init_chunks,
        })?;

        // Every fMP4 segment starts at a random-access point (keyframe
        // bundle boundary), so each CAVS segment is self-sufficient given
        // the init chunks.
        let mut pts_ms = 0u64;
        for (uri, dur_secs) in &playlist_segments {
            let seg_bytes = std::fs::read(workdir.path().join(uri))
                .with_context(|| format!("missing media segment {uri}"))?;
            let chunks = add_chunked(&mut w, &seg_bytes, opts.media_mode())?;
            let dur_ms = (dur_secs * 1000.0).round() as u32;
            w.add_segment(SegmentRecord {
                segment_id,
                track_id,
                pts_start: pts_ms,
                duration: dur_ms,
                flags: SEGMENT_FLAG_RANDOM_ACCESS,
                chunks,
            })?;
            segment_id += 1;
            pts_ms += dur_ms as u64;
        }

        // The playlist itself, as a companion data track, so unpacking can
        // emit a directly playable HLS layout.
        let playlist_chunks = add_chunked(&mut w, playlist_text.as_bytes(), opts.asset_mode())?;
        w.add_track(TrackRecord {
            track_id: PLAYLIST_TRACK_BASE + track_id,
            kind: TrackKind::Data,
            flags: 0,
            codec: "m3u8".to_string(),
            name: format!("{stem}/media.m3u8"),
            timescale: 1000,
            init_chunks: Vec::new(),
        })?;
        w.add_segment(SegmentRecord {
            segment_id,
            track_id: PLAYLIST_TRACK_BASE + track_id,
            pts_start: 0,
            duration: 0,
            flags: SEGMENT_FLAG_RANDOM_ACCESS,
            chunks: playlist_chunks,
        })?;
        segment_id += 1;

        eprintln!(
            "[pack]   {} media segments, total {:.1}s",
            playlist_segments.len(),
            pts_ms as f64 / 1000.0
        );
    }

    let stats = w.finish()?;
    report::print_pack_stats(output, &stats);
    Ok(())
}

pub fn pack_raw(inputs: &[PathBuf], output: &Path, opts: &PackOptions) -> Result<()> {
    let uuid = *uuid::Uuid::new_v4().as_bytes();
    let mut w = Writer::create(output, uuid, 1000, opts.compress)
        .with_context(|| format!("cannot create {}", output.display()))?;
    w.set_zstd_level(opts.zstd_level);
    apply_signing(&mut w, opts)?;
    w.set_meta("packer", concat!("cavs-cli ", env!("CARGO_PKG_VERSION")));
    w.set_meta("payload", "raw");

    let mut used_names = HashSet::new();
    for (i, input) in inputs.iter().enumerate() {
        let name = {
            let file_name = input
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| format!("file-{i}"));
            let mut candidate = file_name.clone();
            let mut n = 1;
            while !used_names.insert(candidate.clone()) {
                n += 1;
                candidate = format!("{n}-{file_name}");
            }
            candidate
        };
        let data =
            std::fs::read(input).with_context(|| format!("cannot read {}", input.display()))?;
        eprintln!(
            "[pack] {} ({} bytes) as raw asset track",
            input.display(),
            data.len()
        );
        // Per-file SHA-256 in meta: lets thin clients (e.g. the Godot
        // GDScript runtime, which has no BLAKE3) verify reconstruction
        // end-to-end with their built-in hasher.
        {
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(&data);
            let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
            w.set_meta(&format!("sha256:{name}"), &hex);
        }
        let (mode, profile_label, kind_label) = resolve_raw_mode(input, &data, opts)?;
        if let Some(label) = profile_label {
            w.set_meta(&format!("profile:{name}"), label);
        }
        if let Some(kind) = kind_label {
            w.set_meta(&format!("payload_kind:{name}"), kind);
        }
        if opts.bootstrap {
            if inputs.len() == 1 {
                write_bootstrap(output, &name, &data, &mut w)?;
            } else if i == 0 {
                eprintln!("[pack] --bootstrap ignored: it requires a single input");
            }
        }
        // v0.6.0: estimate hybrid reuse against a previous version known
        // only through its signature (no old bytes required).
        if let Some(sig_path) = &opts.against_signature {
            let sig_bytes = std::fs::read(sig_path)
                .with_context(|| format!("cannot read {}", sig_path.display()))?;
            let sig = cavs_signature::CavsSignature::decode(&sig_bytes)
                .map_err(|e| anyhow::anyhow!("bad signature {}: {e}", sig_path.display()))?;
            let idx = cavs_signature::diff::WeakHashIndex::build(&sig);
            let plan = cavs_signature::diff::diff_bytes(&idx, &data, Some(&name));
            eprintln!(
                "[pack] vs {}: {:.1}% reusable from the previous install ({} fresh across {} ops)",
                sig.source_label,
                plan.reused_bytes as f64 * 100.0 / data.len().max(1) as f64,
                plan.inline_bytes,
                plan.ops.len(),
            );
        }
        let chunks = add_chunked(&mut w, &data, mode)?;
        let track_id = i as u32 + 1;
        w.add_track(TrackRecord {
            track_id,
            kind: TrackKind::Data,
            flags: 0,
            codec: "raw".to_string(),
            name,
            timescale: 1000,
            init_chunks: Vec::new(),
        })?;
        w.add_segment(SegmentRecord {
            segment_id: i as u64,
            track_id,
            pts_start: 0,
            duration: 0,
            flags: SEGMENT_FLAG_RANDOM_ACCESS,
            chunks,
        })?;
    }

    let stats = w.finish()?;
    report::print_pack_stats(output, &stats);
    Ok(())
}
