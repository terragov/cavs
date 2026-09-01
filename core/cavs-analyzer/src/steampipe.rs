//! The SteamPipe-style fixed-chunk update model.
//!
//! Public SteamPipe documentation describes splitting each file into
//! roughly 1 MiB chunks, compressing them, and reusing chunks that match
//! the previous build during updates. This module reproduces that *public
//! approximation*: old build → fixed chunks → hash table; new build →
//! fixed chunks → download every chunk whose hash is not in the table.
//! It does not model Steam encryption internals or private Valve
//! algorithms.

use crate::walk::{mmap, walk};
use anyhow::Result;
use cavs_hash::{hash_chunk, ChunkHash};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// The documented SteamPipe chunk size (~1 MiB).
pub const DEFAULT_CHUNK: usize = 1024 * 1024;

/// How new-chunk transfer bytes are estimated.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Compression {
    /// Raw chunk bytes (upper bound).
    None,
    /// zstd at this level per chunk (SteamPipe compresses chunks; level 3
    /// is the neutral default used across CAVS benchmarks).
    Zstd(i32),
}

impl Compression {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Compression::None),
            _ => s
                .strip_prefix("zstd-")
                .and_then(|l| l.parse().ok())
                .map(Compression::Zstd),
        }
    }
    pub fn label(&self) -> String {
        match self {
            Compression::None => "none".into(),
            Compression::Zstd(l) => format!("zstd-{l}"),
        }
    }
}

/// Where a new chunk may find a match in the old build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Only against the same path's old chunks (the conservative,
    /// documented per-file model).
    PerFile,
    /// Against every old chunk in the build (content sharing across
    /// files/depots).
    Global,
}

impl Scope {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "per-file" => Some(Scope::PerFile),
            "global" => Some(Scope::Global),
            _ => None,
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Scope::PerFile => "per-file",
            Scope::Global => "global",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ModelConfig {
    pub chunk_size: usize,
    pub compression: Compression,
    pub scope: Scope,
}

impl Default for ModelConfig {
    fn default() -> Self {
        ModelConfig {
            chunk_size: DEFAULT_CHUNK,
            compression: Compression::Zstd(3),
            scope: Scope::PerFile,
        }
    }
}

/// Per-file result of the fixed-chunk model.
#[derive(Serialize, Clone)]
pub struct FileEstimate {
    pub path: String,
    /// new | modified | unchanged
    pub status: String,
    pub is_pack: bool,
    pub old_size: u64,
    pub new_size: u64,
    pub total_chunks: u64,
    pub new_chunks: u64,
    /// 1 − new/total fixed chunks.
    pub reuse_ratio: f64,
    pub download_raw: u64,
    pub download_compressed: u64,
    /// Indices of the changed fixed chunks, for scatteredness analysis.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub changed_chunk_indices: Vec<u32>,
}

/// Whole-transition result of the fixed-chunk model.
#[derive(Serialize, Clone)]
pub struct Estimate {
    pub old_build: String,
    pub new_build: String,
    pub chunk_size: u64,
    pub compression: String,
    pub scope: String,
    pub hash: String,
    pub old_size_bytes: u64,
    pub new_size_bytes: u64,
    pub files_old: usize,
    pub files_new: usize,
    pub files_unchanged: usize,
    pub files_modified: usize,
    pub files_added: usize,
    pub files_deleted: usize,
    pub deleted_paths: Vec<String>,
    pub total_chunks_new: u64,
    pub new_or_changed_chunks: u64,
    pub estimated_download_raw: u64,
    pub estimated_download_compressed: u64,
    /// Local rebuild I/O: SteamPipe builds each touched file alongside the
    /// old one, so the client re-reads and re-writes every touched file in
    /// full even for small changes.
    pub rebuild_read_bytes: u64,
    pub rebuild_write_bytes: u64,
    /// Changed files, sorted by estimated download (largest first).
    pub files: Vec<FileEstimate>,
    pub note: String,
}

impl Estimate {
    /// The file contributing the most estimated download.
    pub fn largest_contributor(&self) -> Option<&FileEstimate> {
        self.files.first()
    }
}

fn fixed_hashes(data: &[u8], chunk: usize) -> Vec<ChunkHash> {
    data.chunks(chunk).map(hash_chunk).collect()
}

fn compressed_len(data: &[u8], compression: Compression) -> u64 {
    match compression {
        Compression::None => data.len() as u64,
        Compression::Zstd(level) => zstd::bulk::compress(data, level)
            .map(|c| c.len() as u64)
            .unwrap_or(data.len() as u64),
    }
}

/// Run the fixed-chunk model over an old→new transition. Both paths may be
/// directories or single artifacts. `keep` filters relative paths (return
/// false to ignore an entry, e.g. from `.cavsignore` rules).
pub fn estimate(
    old_root: &Path,
    new_root: &Path,
    cfg: &ModelConfig,
    keep: &dyn Fn(&str) -> bool,
) -> Result<Estimate> {
    let chunk = cfg.chunk_size.max(1);

    // Artifact mode: two single files are the *same logical artifact*
    // even when their names differ (old.pck vs new.pck), so both sides
    // index under one label.
    let artifact_label: Option<String> = (old_root.is_file() && new_root.is_file()).then(|| {
        new_root
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "artifact".into())
    });
    let relabel = |mut entries: Vec<(String, std::path::PathBuf)>| {
        if let (Some(label), Some(first)) = (&artifact_label, entries.first_mut()) {
            first.0 = label.clone();
        }
        entries
    };

    // Index the old build deterministically.
    let mut per_file: HashMap<String, HashSet<ChunkHash>> = HashMap::new();
    let mut global: HashSet<ChunkHash> = HashSet::new();
    let mut old_sizes: HashMap<String, u64> = HashMap::new();
    let mut old_total = 0u64;
    for (rel, abs) in relabel(walk(old_root)?)
        .into_iter()
        .filter(|(r, _)| keep(r))
    {
        let Some(map) = mmap(&abs)? else {
            per_file.insert(rel.clone(), HashSet::new());
            old_sizes.insert(rel, 0);
            continue;
        };
        old_total += map.len() as u64;
        old_sizes.insert(rel.clone(), map.len() as u64);
        let hashes: HashSet<ChunkHash> = fixed_hashes(&map, chunk).into_iter().collect();
        if cfg.scope == Scope::Global {
            global.extend(&hashes);
        }
        per_file.insert(rel, hashes);
    }

    // Walk the new build and count chunks with no match.
    let mut files: Vec<FileEstimate> = Vec::new();
    let mut new_total = 0u64;
    let mut total_chunks = 0u64;
    let mut new_chunks = 0u64;
    let mut download_raw = 0u64;
    let mut download_compressed = 0u64;
    let mut rebuild_read = 0u64;
    let mut rebuild_write = 0u64;
    let mut unchanged = 0usize;
    let mut seen_paths: HashSet<String> = HashSet::new();
    let empty = HashSet::new();

    for (rel, abs) in relabel(walk(new_root)?)
        .into_iter()
        .filter(|(r, _)| keep(r))
    {
        seen_paths.insert(rel.clone());
        let map = mmap(&abs)?;
        let (data_len, hashes) = match &map {
            Some(m) => (m.len(), fixed_hashes(m, chunk)),
            None => (0, Vec::new()),
        };
        new_total += data_len as u64;
        total_chunks += hashes.len() as u64;

        let old_here = per_file.get(&rel);
        let status = if old_here.is_none() {
            "new"
        } else {
            "modified"
        };
        let lookup: &HashSet<ChunkHash> = match cfg.scope {
            Scope::PerFile => old_here.unwrap_or(&empty),
            Scope::Global => &global,
        };

        let mut file_new = 0u64;
        let mut file_raw = 0u64;
        let mut file_compressed = 0u64;
        let mut changed_idx: Vec<u32> = Vec::new();
        if let Some(m) = &map {
            let mut off = 0usize;
            for (i, h) in hashes.iter().enumerate() {
                let len = chunk.min(data_len - off);
                if !lookup.contains(h) {
                    file_new += 1;
                    file_raw += len as u64;
                    file_compressed += compressed_len(&m[off..off + len], cfg.compression);
                    changed_idx.push(i as u32);
                }
                off += len;
            }
        }

        // Unchanged file: identical size and every chunk matched same-path.
        let same_path_match = old_here.is_some_and(|of| {
            old_sizes.get(&rel) == Some(&(data_len as u64))
                && hashes.iter().all(|h| of.contains(h))
                && of.len() == hashes.len()
        });
        if same_path_match {
            unchanged += 1;
            continue;
        }

        new_chunks += file_new;
        download_raw += file_raw;
        download_compressed += file_compressed;
        // Touched file: read the old copy + write the new copy in full.
        rebuild_read += old_sizes.get(&rel).copied().unwrap_or(0);
        rebuild_write += data_len as u64;

        let total = hashes.len().max(1) as u64;
        files.push(FileEstimate {
            path: rel.clone(),
            status: status.into(),
            is_pack: crate::is_pack(&rel),
            old_size: old_sizes.get(&rel).copied().unwrap_or(0),
            new_size: data_len as u64,
            total_chunks: hashes.len() as u64,
            new_chunks: file_new,
            reuse_ratio: 1.0 - file_new as f64 / total as f64,
            download_raw: file_raw,
            download_compressed: file_compressed,
            changed_chunk_indices: changed_idx,
        });
    }

    let deleted_paths: Vec<String> = {
        let mut d: Vec<String> = old_sizes
            .keys()
            .filter(|p| !seen_paths.contains(*p))
            .cloned()
            .collect();
        d.sort();
        d
    };

    files.sort_by_key(|f| std::cmp::Reverse(f.download_compressed));
    let files_added = files.iter().filter(|f| f.status == "new").count();
    let files_modified = files.iter().filter(|f| f.status == "modified").count();

    Ok(Estimate {
        old_build: old_root.display().to_string(),
        new_build: new_root.display().to_string(),
        chunk_size: chunk as u64,
        compression: cfg.compression.label(),
        scope: cfg.scope.label().into(),
        hash: "blake3".into(),
        old_size_bytes: old_total,
        new_size_bytes: new_total,
        files_old: old_sizes.len(),
        files_new: seen_paths.len(),
        files_unchanged: unchanged,
        files_modified,
        files_added,
        files_deleted: deleted_paths.len(),
        deleted_paths,
        total_chunks_new: total_chunks,
        new_or_changed_chunks: new_chunks,
        estimated_download_raw: download_raw,
        estimated_download_compressed: download_compressed,
        rebuild_read_bytes: rebuild_read,
        rebuild_write_bytes: rebuild_write,
        files,
        note: crate::ESTIMATE_NOTE.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keep_all(_: &str) -> bool {
        true
    }

    fn pseudo_random(len: usize, seed: u32) -> Vec<u8> {
        let mut out = vec![0u8; len];
        let mut state = seed;
        for b in out.iter_mut() {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            *b = (state >> 24) as u8;
        }
        out
    }

    #[test]
    fn identical_builds_cost_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        for root in [&a, &b] {
            std::fs::create_dir_all(root).unwrap();
            std::fs::write(root.join("x.bin"), pseudo_random(3 << 20, 7)).unwrap();
        }
        let e = estimate(&a, &b, &ModelConfig::default(), &keep_all).unwrap();
        assert_eq!(e.new_or_changed_chunks, 0);
        assert_eq!(e.estimated_download_compressed, 0);
        assert_eq!(e.files_unchanged, 1);
        assert!(e.files.is_empty());
    }

    #[test]
    fn localized_change_costs_one_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let base = pseudo_random(4 << 20, 9);
        let mut changed = base.clone();
        changed[2 << 20] ^= 0xff; // one byte in the third 1 MiB chunk
        std::fs::write(a.join("x.bin"), &base).unwrap();
        std::fs::write(b.join("x.bin"), &changed).unwrap();
        let e = estimate(&a, &b, &ModelConfig::default(), &keep_all).unwrap();
        assert_eq!(e.new_or_changed_chunks, 1);
        assert_eq!(e.files[0].changed_chunk_indices, vec![2]);
        // rebuild I/O covers the whole touched file, not just the chunk
        assert_eq!(e.rebuild_write_bytes, (4 << 20) as u64);
    }

    #[test]
    fn shifted_content_defeats_fixed_chunks_per_file() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let base = pseudo_random(4 << 20, 11);
        let mut shifted = vec![0xAAu8; 4096];
        shifted.extend_from_slice(&base);
        std::fs::write(a.join("x.bin"), &base).unwrap();
        std::fs::write(b.join("x.bin"), &shifted).unwrap();
        let e = estimate(&a, &b, &ModelConfig::default(), &keep_all).unwrap();
        // Every fixed window slides: nothing reusable.
        assert_eq!(e.new_or_changed_chunks, e.total_chunks_new);
    }

    #[test]
    fn global_scope_finds_moved_files() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let data = pseudo_random(2 << 20, 13);
        std::fs::write(a.join("old_name.bin"), &data).unwrap();
        std::fs::write(b.join("new_name.bin"), &data).unwrap();

        let per_file = estimate(&a, &b, &ModelConfig::default(), &keep_all).unwrap();
        assert_eq!(per_file.new_or_changed_chunks, 2);

        let global = estimate(
            &a,
            &b,
            &ModelConfig {
                scope: Scope::Global,
                ..Default::default()
            },
            &keep_all,
        )
        .unwrap();
        assert_eq!(global.new_or_changed_chunks, 0);
    }

    #[test]
    fn artifact_mode_pairs_files_with_different_names() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.pck");
        let new = dir.path().join("new.pck");
        let base = pseudo_random(3 << 20, 21);
        let mut changed = base.clone();
        changed[0] ^= 1;
        std::fs::write(&old, &base).unwrap();
        std::fs::write(&new, &changed).unwrap();
        let e = estimate(&old, &new, &ModelConfig::default(), &keep_all).unwrap();
        // One changed chunk — not a brand-new 3 MiB file.
        assert_eq!(e.new_or_changed_chunks, 1);
        assert_eq!(e.files_modified, 1);
        assert_eq!(e.files_added, 0);
        assert_eq!(e.files[0].old_size, (3 << 20) as u64);
    }

    #[test]
    fn deleted_files_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("gone.bin"), b"bye").unwrap();
        std::fs::write(a.join("kept.bin"), b"hello").unwrap();
        std::fs::write(b.join("kept.bin"), b"hello").unwrap();
        let e = estimate(&a, &b, &ModelConfig::default(), &keep_all).unwrap();
        assert_eq!(e.deleted_paths, vec!["gone.bin".to_string()]);
    }

    #[test]
    fn config_parsing() {
        assert_eq!(Compression::parse("none"), Some(Compression::None));
        assert_eq!(Compression::parse("zstd-19"), Some(Compression::Zstd(19)));
        assert_eq!(Compression::parse("lz4"), None);
        assert_eq!(Scope::parse("global"), Some(Scope::Global));
        assert_eq!(Scope::parse("per-file"), Some(Scope::PerFile));
    }
}
