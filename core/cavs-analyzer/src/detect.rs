//! Failure-mode detectors and the recommendation engine.
//!
//! Every detector consumes the per-file signals gathered by
//! [`crate::compare`] and yields [`Finding`]s: severity, the affected
//! file, an estimate of wasted bytes, why it happens, the recommended fix
//! and the expected improvement. Thresholds live in [`Thresholds`] so
//! tests and callers can tighten or relax them.

use crate::entropy::HIGH_ENTROPY;
use crate::human_bytes;
use crate::windows::Heatmap;
use serde::Serialize;

#[derive(Serialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }
}

/// One detected issue plus its recommendation.
#[derive(Serialize, Clone)]
pub struct Finding {
    pub severity: Severity,
    /// Stable machine key: scattered_pack_churn, asset_shuffling,
    /// toc_churn, compressed_blob, metadata_churn, oversized_pack,
    /// new_content_in_old_pack.
    pub kind: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    pub estimated_wasted_bytes: u64,
    pub why: String,
    pub fix: String,
    pub expected_improvement: String,
}

/// Signals for one changed file, gathered once and shared by every
/// detector.
pub struct FileSignals {
    pub path: String,
    /// new | modified
    pub status: String,
    pub is_pack: bool,
    pub old_size: u64,
    pub new_size: u64,
    /// Fixed 1 MiB same-path chunk reuse (SteamPipe-style model).
    pub fixed_reuse: f64,
    /// FastCDC global reuse (content similarity independent of offsets).
    pub cdc_reuse: f64,
    /// Estimated SteamPipe-style download for this file (compressed).
    pub steam_download: u64,
    /// Estimated content-defined download for this file (compressed).
    pub cdc_download: u64,
    /// Sampled Shannon entropy of the new bytes, bits/byte.
    pub entropy: f64,
    /// Positional heatmap at 64 KiB windows.
    pub heat_64k: Heatmap,
    /// Positional heatmap at 1 MiB windows.
    pub heat_1m: Heatmap,
}

/// Detector thresholds; the defaults follow the v0.9.0 plan.
pub struct Thresholds {
    /// Packs above these sizes get advisory/warning/critical size findings.
    pub pack_advisory: u64,
    pub pack_warning: u64,
    pub pack_critical: u64,
    /// Minimum size before scattered/shuffling/TOC findings apply.
    pub min_interesting: u64,
    /// Scatteredness (at 1 MiB windows) above this is "scattered".
    pub scattered: f64,
    /// Minimum changed 1 MiB windows for a scattered-churn finding.
    pub scattered_min_windows: u64,
    /// CDC reuse must exceed fixed reuse by this much for shuffling.
    pub shuffle_gap: f64,
    /// TOC churn: at least this many isolated runs at 64 KiB windows...
    pub toc_min_runs: u64,
    /// ...with mean run length at or below this...
    pub toc_max_mean_run: f64,
    /// ...density below this, spread over at least this span.
    pub toc_max_density: f64,
    pub toc_min_span: f64,
    /// Same-size files whose change fits in this many 64 KiB windows
    /// count towards the metadata-churn build finding.
    pub metadata_max_windows: u64,
    /// Minimum such files before metadata churn is reported.
    pub metadata_min_files: usize,
    /// Growth of a modified pack that suggests new content was packed
    /// into it instead of shipping as a new pack.
    pub new_content_growth: u64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            pack_advisory: 1 << 30,
            pack_warning: 2 << 30,
            pack_critical: 8u64 << 30,
            min_interesting: 16 << 20,
            scattered: 0.5,
            scattered_min_windows: 16,
            shuffle_gap: 0.25,
            toc_min_runs: 24,
            toc_max_mean_run: 2.0,
            toc_max_density: 0.15,
            toc_min_span: 0.5,
            metadata_max_windows: 2,
            metadata_min_files: 5,
            new_content_growth: 16 << 20,
        }
    }
}

/// Bytes the fixed-chunk model wastes versus what the content actually
/// requires (the content-defined estimate as the proxy for "real" new
/// data).
fn wasted(f: &FileSignals) -> u64 {
    f.steam_download.saturating_sub(f.cdc_download)
}

/// Run every per-file detector.
pub fn detect_file(f: &FileSignals, t: &Thresholds) -> Vec<Finding> {
    let mut out = Vec::new();

    // 2.6 Oversized pack files.
    if f.is_pack {
        let (sev, floor) = if f.new_size > t.pack_critical {
            (Some(Severity::Critical), t.pack_critical)
        } else if f.new_size > t.pack_warning {
            (Some(Severity::Warning), t.pack_warning)
        } else if f.new_size > t.pack_advisory {
            (Some(Severity::Info), t.pack_advisory)
        } else {
            (None, 0)
        };
        if let Some(severity) = sev {
            out.push(Finding {
                severity,
                kind: "oversized_pack".into(),
                title: format!("Pack file over {}", human_bytes(floor)),
                file: Some(f.path.clone()),
                estimated_wasted_bytes: 0,
                why: format!(
                    "{} is {}. Fixed-chunk updaters rebuild a touched pack alongside the \
                     old copy, so even a tiny change re-reads and re-writes the whole \
                     file on the client's disk.",
                    f.path,
                    human_bytes(f.new_size)
                ),
                fix: "Split the pack by level/feature into 1–2 GiB parts aligned to \
                      update cadence."
                    .into(),
                expected_improvement: "Local update I/O drops from the whole pack to \
                                       only the parts that changed."
                    .into(),
            });
        }
    }

    if f.status != "modified" || f.new_size < t.min_interesting {
        return out;
    }

    // 2.3 Distributed TOC / absolute offset churn — check before the
    // generic scattered finding: it is the more specific diagnosis.
    let h = &f.heat_64k;
    let is_toc = h.runs >= t.toc_min_runs
        && h.mean_run_len <= t.toc_max_mean_run
        && h.changed_byte_density < t.toc_max_density
        && h.span_ratio >= t.toc_min_span;
    if is_toc {
        out.push(Finding {
            severity: Severity::Critical,
            kind: "toc_churn".into(),
            title: "Distributed TOC / absolute-offset churn".into(),
            file: Some(f.path.clone()),
            estimated_wasted_bytes: wasted(f),
            why: format!(
                "{} changed in {} tiny regions (mean run {:.1} windows of 64 KiB, byte \
                 density {:.1}%) spread over {:.0}% of the file — the signature of \
                 table-of-contents entries or absolute offsets rewritten throughout \
                 the pack.",
                f.path,
                h.runs,
                h.mean_run_len,
                h.changed_byte_density * 100.0,
                h.span_ratio * 100.0
            ),
            fix: "Move the TOC to the beginning or end of the pack, use relative \
                  offsets, keep asset ordering stable and avoid repacking unrelated \
                  assets."
                .into(),
            expected_improvement: format!(
                "Most of the ~{} of dirtied 1 MiB chunks would survive; only the \
                 real edits and one TOC region would ship.",
                human_bytes(f.steam_download)
            ),
        });
    }

    // 2.1 Scattered pack-file churn.
    let h1 = &f.heat_1m;
    if !is_toc
        && f.is_pack
        && h1.changed_windows >= t.scattered_min_windows
        && h1.scatteredness >= t.scattered
    {
        out.push(Finding {
            severity: if f.new_size > t.pack_advisory {
                Severity::Critical
            } else {
                Severity::Warning
            },
            kind: "scattered_pack_churn".into(),
            title: "Scattered changes across a pack file".into(),
            file: Some(f.path.clone()),
            estimated_wasted_bytes: wasted(f),
            why: format!(
                "{} changed in {} of {} 1 MiB windows across {} runs \
                 (scatteredness {:.2}). Fixed 1 MiB chunking cannot reuse windows \
                 whose content moved or interleaves edits.",
                f.path, h1.changed_windows, h1.total_windows, h1.runs, h1.scatteredness
            ),
            fix: "Group assets by level/feature, keep asset ordering stable between \
                  builds and add new content as new packs."
                .into(),
            expected_improvement: "Changes collapse into few contiguous regions, so \
                                   fixed-chunk updates only ship those regions."
                .into(),
        });
    }

    // 2.2 Asset shuffling: content is present, offsets moved.
    if f.cdc_reuse - f.fixed_reuse > t.shuffle_gap {
        out.push(Finding {
            severity: Severity::Critical,
            kind: "asset_shuffling".into(),
            title: "Assets shifted or reordered inside the file".into(),
            file: Some(f.path.clone()),
            estimated_wasted_bytes: wasted(f),
            why: format!(
                "{} keeps {:.0}% of its content (content-defined chunks) but only \
                 {:.0}% of its fixed 1 MiB chunks — the bytes are there, at different \
                 offsets. Typical causes: reordered assets, a grown asset shifting \
                 everything after it, or non-deterministic packing.",
                f.path,
                f.cdc_reuse * 100.0,
                f.fixed_reuse * 100.0
            ),
            fix: "Keep a stable asset order, pad or align entries so unrelated assets \
                  keep their offsets, and avoid full repacks for small changes."
                .into(),
            expected_improvement: format!(
                "Up to {} of the estimated download is misalignment, not new \
                 content, and would disappear with stable offsets.",
                human_bytes(wasted(f))
            ),
        });
    }

    // 2.4 Compression/encryption across asset boundaries.
    if f.entropy >= HIGH_ENTROPY && f.fixed_reuse < 0.20 && f.cdc_reuse < 0.25 {
        out.push(Finding {
            severity: Severity::Warning,
            kind: "compressed_blob".into(),
            title: "Compression crossing asset boundaries".into(),
            file: Some(f.path.clone()),
            estimated_wasted_bytes: f.steam_download,
            why: format!(
                "{} has entropy {:.2} bits/byte and near-zero chunk reuse under both \
                 the fixed and the content-defined model. That is the shape of a \
                 globally compressed (or encrypted) stream: any source change \
                 cascades through the rest of the file.",
                f.path, f.entropy
            ),
            fix: "Compress per asset instead of per pack (or disable pack-level \
                  compression) so asset boundaries stay stable; keep encryption per \
                  asset if the engine allows it."
                .into(),
            expected_improvement: "Only the assets that actually changed would ship, \
                                   instead of everything after the first edit."
                .into(),
        });
    }

    out
}

/// Build-level detectors that need to see every file at once.
pub fn detect_build(files: &[FileSignals], t: &Thresholds) -> Vec<Finding> {
    let mut out = Vec::new();

    // 2.5 Timestamps / build IDs: many same-size files with tiny edits.
    let metadata_suspects: Vec<&FileSignals> = files
        .iter()
        .filter(|f| {
            f.status == "modified"
                && f.old_size == f.new_size
                && f.heat_64k.changed_windows > 0
                && f.heat_64k.changed_windows <= t.metadata_max_windows
        })
        .collect();
    if metadata_suspects.len() >= t.metadata_min_files {
        let total: u64 = metadata_suspects.iter().map(|f| f.steam_download).sum();
        let sample: Vec<&str> = metadata_suspects
            .iter()
            .take(3)
            .map(|f| f.path.as_str())
            .collect();
        out.push(Finding {
            severity: Severity::Warning,
            kind: "metadata_churn".into(),
            title: "Timestamps or build IDs dirtying many files".into(),
            file: None,
            estimated_wasted_bytes: total,
            why: format!(
                "{} files kept their exact size but changed in at most {} small \
                 windows each (e.g. {}). That pattern is embedded timestamps, build \
                 IDs or generated names — every release dirties chunks that carry no \
                 real content.",
                metadata_suspects.len(),
                t.metadata_max_windows,
                sample.join(", ")
            ),
            fix: "Strip or pin timestamps/build IDs at export time and make the \
                  build deterministic."
                .into(),
            expected_improvement: format!(
                "~{} per release stops shipping for free.",
                human_bytes(total)
            ),
        });
    }

    // 2.7 New content placed inside an old pack.
    let new_file_bytes: u64 = files
        .iter()
        .filter(|f| f.status == "new")
        .map(|f| f.new_size)
        .sum();
    for f in files {
        if f.is_pack
            && f.status == "modified"
            && f.new_size > f.old_size
            && f.new_size - f.old_size >= t.new_content_growth
            && new_file_bytes < (f.new_size - f.old_size) / 4
        {
            let growth = f.new_size - f.old_size;
            out.push(Finding {
                severity: Severity::Warning,
                kind: "new_content_in_old_pack".into(),
                title: "New content added inside an existing pack".into(),
                file: Some(f.path.clone()),
                estimated_wasted_bytes: f.steam_download.saturating_sub(growth),
                why: format!(
                    "{} grew by {} while the build gained almost no new files — new \
                     content was packed into the existing pack, dirtying its layout, \
                     instead of shipping as a separate pack.",
                    f.path,
                    human_bytes(growth)
                ),
                fix: "Ship new levels/features as new pack files; keep released \
                      packs immutable."
                    .into(),
                expected_improvement: "The update approaches the size of the new \
                                       content itself; old packs stay fully reusable."
                    .into(),
            });
        }
    }

    out.sort_by_key(|f| std::cmp::Reverse(f.severity));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::heatmap;

    fn signals(path: &str, old: &[u8], new: &[u8]) -> FileSignals {
        FileSignals {
            path: path.into(),
            status: "modified".into(),
            is_pack: crate::is_pack(path),
            old_size: old.len() as u64,
            new_size: new.len() as u64,
            fixed_reuse: 0.0,
            cdc_reuse: 0.0,
            steam_download: new.len() as u64,
            cdc_download: 0,
            entropy: 0.0,
            heat_64k: heatmap(old, new, 64 * 1024),
            heat_1m: heatmap(old, new, 1024 * 1024),
        }
    }

    fn small_thresholds() -> Thresholds {
        Thresholds {
            min_interesting: 1 << 20,
            ..Default::default()
        }
    }

    #[test]
    fn toc_churn_detected() {
        let old = vec![3u8; 8 << 20];
        let mut new = old.clone();
        let w = 64 * 1024;
        for i in (0..new.len() / w).step_by(4) {
            new[i * w + 8] ^= 0x55; // one dirty byte per fourth window
        }
        let mut s = signals("world.pak", &old, &new);
        s.fixed_reuse = 0.1;
        s.cdc_reuse = 0.2;
        let findings = detect_file(&s, &small_thresholds());
        assert!(
            findings.iter().any(|f| f.kind == "toc_churn"),
            "kinds: {:?}",
            findings.iter().map(|f| f.kind.clone()).collect::<Vec<_>>()
        );
        // TOC diagnosis suppresses the generic scattered finding.
        assert!(!findings.iter().any(|f| f.kind == "scattered_pack_churn"));
    }

    #[test]
    fn shuffling_detected_from_reuse_gap() {
        let old = vec![1u8; 2 << 20];
        let new = vec![1u8; 2 << 20];
        let mut s = signals("data.pck", &old, &new);
        s.fixed_reuse = 0.10;
        s.cdc_reuse = 0.95;
        s.steam_download = 10 << 20;
        s.cdc_download = 1 << 20;
        let findings = detect_file(&s, &small_thresholds());
        let f = findings
            .iter()
            .find(|f| f.kind == "asset_shuffling")
            .unwrap();
        assert_eq!(f.severity, Severity::Critical);
        assert_eq!(f.estimated_wasted_bytes, 9 << 20);
    }

    #[test]
    fn compressed_blob_detected() {
        let old = vec![1u8; 2 << 20];
        let new = vec![2u8; 2 << 20];
        let mut s = signals("archive.zip", &old, &new);
        s.entropy = 7.9;
        s.fixed_reuse = 0.02;
        s.cdc_reuse = 0.05;
        let findings = detect_file(&s, &small_thresholds());
        assert!(findings.iter().any(|f| f.kind == "compressed_blob"));
    }

    #[test]
    fn oversized_pack_advisories() {
        let t = Thresholds::default();
        let mut s = signals("big.pak", &[], &[]);
        s.status = "new".into();
        for (size, expect) in [
            (512u64 << 20, None),
            (1500 << 20, Some(Severity::Info)),
            (3 << 30, Some(Severity::Warning)),
            (9u64 << 30, Some(Severity::Critical)),
        ] {
            s.new_size = size;
            let found = detect_file(&s, &t)
                .into_iter()
                .find(|f| f.kind == "oversized_pack");
            assert_eq!(found.map(|f| f.severity), expect, "size {size}");
        }
    }

    #[test]
    fn metadata_churn_needs_many_small_same_size_edits() {
        let w = 64 * 1024;
        let old = vec![5u8; 4 * w];
        let mut new = old.clone();
        new[10] ^= 1;
        let mut files = Vec::new();
        for i in 0..6 {
            let mut s = signals(&format!("bin/lib{i}.dll"), &old, &new);
            s.steam_download = 1 << 20;
            files.push(s);
        }
        let findings = detect_build(&files, &Thresholds::default());
        let f = findings
            .iter()
            .find(|f| f.kind == "metadata_churn")
            .unwrap();
        assert_eq!(f.estimated_wasted_bytes, 6 << 20);

        // Below the file-count threshold: silent.
        let findings = detect_build(&files[..3], &Thresholds::default());
        assert!(findings.iter().all(|f| f.kind != "metadata_churn"));
    }

    #[test]
    fn new_content_in_old_pack_detected() {
        let old = vec![1u8; 4 << 20];
        let mut new = old.clone();
        new.extend(vec![9u8; 32 << 20]);
        let s = signals("world.pak", &old, &new);
        let findings = detect_build(&[s], &Thresholds::default());
        assert!(findings.iter().any(|f| f.kind == "new_content_in_old_pack"));
    }
}
