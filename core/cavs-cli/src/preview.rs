//! `cavs preview` — what changed between a new build and the previous
//! version's `.cavssig`, before packaging or publishing anything.
//!
//! Classifies every entry (NEW / MODIFIED / DELETED / SAME), estimates the
//! update cost per delivery route and warns about compressed/high-entropy
//! files that defeat block-level patching.

use crate::compare::{classify, EntryReport, FileState};
use crate::report::human_bytes;
use anyhow::{Context, Result};
use cavs_signature::diff::{diff_bytes, WeakHashIndex};
use std::path::Path;

#[derive(Default, serde::Serialize)]
struct PreviewReport {
    new_build: String,
    against: String,
    entries: Vec<EntryReport>,
    summary: Summary,
    warnings: Vec<String>,
    /// new path → old path (same content, different location).
    renames: std::collections::HashMap<String, String>,
}

#[derive(Default, serde::Serialize)]
struct Summary {
    new: u64,
    modified: u64,
    deleted: u64,
    same: u64,
    new_build_bytes: u64,
    /// Bytes in NEW + MODIFIED files (what any patcher must look at).
    changed_file_bytes: u64,
    /// Estimated CAVS update: fresh bytes after block-level reuse against
    /// the signature, zstd-3 compressed (chunk route and hybrid route move
    /// the same changed content; hybrid just sources the rest locally).
    estimated_cavs_update_bytes: u64,
    /// Estimated full re-download: whole new build, zstd-3.
    estimated_full_zstd_bytes: u64,
    /// Raw fresh bytes before compression (inline data).
    fresh_bytes: u64,
    reused_bytes: u64,
}

pub fn preview(
    new_build: &Path,
    against: &Path,
    changes_only: bool,
    detect_blobs: bool,
    json: bool,
) -> Result<()> {
    let sig = crate::signature_cmd::load(against)?;
    let entries = classify(&sig, new_build)?;
    let index = WeakHashIndex::build(&sig);

    let mut report = PreviewReport {
        new_build: new_build.display().to_string(),
        against: against.display().to_string(),
        entries: Vec::new(),
        summary: Summary::default(),
        warnings: Vec::new(),
        renames: crate::compare::detect_renames(&sig, new_build, &entries)?,
    };

    let mut inline_total: Vec<u8> = Vec::new();
    let mut full_zstd = 0u64;
    for e in &entries {
        match e.state {
            FileState::New => report.summary.new += 1,
            FileState::Modified => report.summary.modified += 1,
            FileState::Deleted => report.summary.deleted += 1,
            FileState::Same => report.summary.same += 1,
        }
        if e.state == FileState::Deleted {
            continue;
        }
        report.summary.new_build_bytes += e.size;

        let full = if sig.kind == cavs_signature::SignatureKind::SingleArtifact {
            new_build.to_path_buf()
        } else {
            new_build.join(&e.path)
        };
        if !full.is_file() {
            continue; // dirs, symlinks
        }
        let bytes = std::fs::read(&full).with_context(|| format!("reading {}", full.display()))?;
        full_zstd += zstd::bulk::compress(&bytes, 3).context("zstd")?.len() as u64;

        match e.state {
            FileState::Same => {
                report.summary.reused_bytes += e.size;
            }
            FileState::New | FileState::Modified => {
                report.summary.changed_file_bytes += e.size;
                let target = (sig.kind == cavs_signature::SignatureKind::DirectoryContainer)
                    .then_some(e.path.as_str());
                let diff = diff_bytes(&index, &bytes, target);
                report.summary.reused_bytes += diff.reused_bytes;
                report.summary.fresh_bytes += diff.inline_bytes;
                for op in &diff.ops {
                    if let cavs_signature::diff::DiffOp::InlineData { new_offset, len } = op {
                        inline_total.extend_from_slice(
                            &bytes[*new_offset as usize..(*new_offset + *len) as usize],
                        );
                    }
                }
                warn_if_patch_hostile(&mut report.warnings, e, &bytes, diff.reused_bytes);
                if detect_blobs {
                    warn_if_compressed_blob(
                        &mut report.warnings,
                        e,
                        &bytes,
                        diff.inline_bytes,
                        report.renames.contains_key(&e.path),
                    );
                }
            }
            FileState::Deleted => unreachable!(),
        }
    }
    report.summary.estimated_cavs_update_bytes = if inline_total.is_empty() {
        0
    } else {
        zstd::bulk::compress(&inline_total, 3)
            .context("zstd")?
            .len() as u64
    };
    report.summary.estimated_full_zstd_bytes = full_zstd;
    report.entries = entries;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    for e in &report.entries {
        if changes_only && e.state == FileState::Same {
            continue;
        }
        let rename_note = report
            .renames
            .get(&e.path)
            .map(|from| format!("  (renamed from {from} — no payload)"))
            .unwrap_or_default();
        println!(
            "{:<9} {:>12}  {}{}",
            e.state.label(),
            human_bytes(e.size),
            e.path,
            rename_note
        );
    }
    let s = &report.summary;
    println!("\nSummary:");
    println!("  new      : {}", s.new);
    println!("  modified : {}", s.modified);
    println!("  deleted  : {}", s.deleted);
    println!("  same     : {}", s.same);
    println!(
        "  estimated CAVS update    : {} (fresh {} of {}, block-level reuse {})",
        human_bytes(s.estimated_cavs_update_bytes),
        human_bytes(s.fresh_bytes),
        human_bytes(s.new_build_bytes),
        human_bytes(s.reused_bytes),
    );
    println!(
        "  estimated full zstd-3    : {}",
        human_bytes(s.estimated_full_zstd_bytes)
    );
    for w in &report.warnings {
        println!("\nWARNING:\n  {w}");
    }
    Ok(())
}

/// Explicit archive/high-entropy detection (`--detect-compressed-blobs`):
/// magic bytes catch archives even before they change much, and the
/// warning quantifies what an update through the blob costs.
fn warn_if_compressed_blob(
    warnings: &mut Vec<String>,
    e: &EntryReport,
    bytes: &[u8],
    fresh_bytes: u64,
    renamed: bool,
) {
    const MIN_SIZE: u64 = 256 * 1024;
    if e.size < MIN_SIZE || renamed {
        return;
    }
    let (shape, magic) = crate::blob_detect::classify_blob(bytes);
    if shape == crate::blob_detect::BlobShape::Plain {
        return;
    }
    // High-entropy fresh bytes barely compress: the inline size *is* the cost.
    warnings.push(format!(
        "{} is a {} blob ({}). Estimated update cost through the blob: ~{} \
         (block-level reuse found only {}). Publish the uncompressed folder \
         (directory mode), or route this pair through an optimized sidecar \
         (cavs optimize-patch picks a byte-level delta for it).",
        e.path,
        magic.unwrap_or(shape.label()),
        human_bytes(e.size),
        human_bytes(fresh_bytes),
        human_bytes(e.size.saturating_sub(fresh_bytes)),
    ));
}

/// A large file that changed almost everywhere and does not compress is a
/// compressed/encrypted container: one source edit cascades over the whole
/// output and no block patcher can help. Tell the developer now, not
/// after clients download it.
fn warn_if_patch_hostile(warnings: &mut Vec<String>, e: &EntryReport, bytes: &[u8], reused: u64) {
    const MIN_SIZE: u64 = 1024 * 1024;
    if e.state != FileState::Modified || e.size < MIN_SIZE {
        return;
    }
    let changed_pct = 100 - (reused * 100 / e.size.max(1));
    if changed_pct < 75 {
        return;
    }
    let sample = &bytes[..bytes.len().min(256 * 1024)];
    let ratio = zstd::bulk::compress(sample, 3)
        .map(|c| c.len() as f64 / sample.len().max(1) as f64)
        .unwrap_or(1.0);
    if ratio > 0.98 {
        warnings.push(format!(
            "{} changed {changed_pct}% of its bytes and looks compressed/high-entropy. \
             Small source changes cascade across compressed output; consider publishing \
             the uncompressed folder (directory mode) or a stable, content-addressed export.",
            e.path
        ));
    } else {
        warnings.push(format!(
            "{} changed {changed_pct}% of its bytes at block level. \
             Try stable pack ordering or content-addressed asset export to improve reuse.",
            e.path
        ));
    }
}
