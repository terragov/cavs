//! `cavs bench gen` / `cavs bench suite` — synthetic large-build
//! benchmarks (v0.5.0).
//!
//! `gen` produces a deterministic dataset (same seed + size ⇒ identical
//! bytes on any machine): a base build `v1.bin` plus the update shapes
//! that matter for chunked delivery:
//!
//! - `v2-small`    ~3% of blocks changed
//! - `v2-medium`   ~15% changed
//! - `v2-large`    ~50% changed
//! - `v2-shifted`  4 KiB inserted at the head (every byte shifts)
//! - `v2-reordered` same blocks, halves swapped in 8 MiB groups
//!
//! Content is a 50/50 mix of compressible (patterned) and incompressible
//! (PRNG) 64 KiB blocks, which is roughly how real builds behave.
//! Everything streams block-by-block, so datasets larger than RAM are fine
//! to *generate*; `suite` packs each version (FastCDC 64 KiB + zstd 3),
//! measures pack time, container/manifest sizes, dedup and update egress,
//! and writes `summary.md` + `summary.json`.

use crate::pack::{self, PackOptions};
use crate::report::human_bytes;
use crate::ChunkModeArg;
use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};

const BLOCK: usize = 64 * 1024;
/// Reordering swaps halves within groups of this many blocks (8 MiB).
const REORDER_GROUP: u64 = 128;

/// xorshift64*: tiny, fast, deterministic across platforms.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }
    pub(crate) fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
}

/// The content of block `index` for a given generation `salt`. Even
/// blocks are compressible (repeating 32-byte pattern), odd blocks are
/// PRNG noise — a 50/50 mix. Changing the salt changes the bytes.
pub(crate) fn block_bytes(seed: u64, salt: u64, index: u64) -> Vec<u8> {
    let mut rng = Rng::new(seed ^ salt.wrapping_mul(0x9E3779B97F4A7C15) ^ index.rotate_left(17));
    let mut out = vec![0u8; BLOCK];
    if index.is_multiple_of(2) {
        let mut pattern = [0u8; 32];
        for b in pattern.iter_mut() {
            *b = rng.next() as u8;
        }
        for (i, b) in out.iter_mut().enumerate() {
            *b = pattern[i % 32];
        }
    } else {
        for chunk in out.chunks_mut(8) {
            let v = rng.next().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }
    out
}

pub fn generate(out: &Path, size: &str, seed: u64) -> Result<()> {
    let total = parse_size(size)?;
    let blocks = total.div_ceil(BLOCK as u64).max(2);
    std::fs::create_dir_all(out)?;

    // Which blocks change per variant: deterministic from the seed.
    let changed_set = |pct: u64| -> HashSet<u64> {
        let mut rng = Rng::new(seed.wrapping_mul(31).wrapping_add(pct));
        let target = (blocks * pct / 100).max(1);
        let mut set = HashSet::new();
        while (set.len() as u64) < target {
            set.insert(rng.next() % blocks);
        }
        set
    };

    let write_variant =
        |name: &str, changed: &HashSet<u64>, head_insert: bool, reorder: bool| -> Result<u64> {
            let path = out.join(name);
            let mut file = std::io::BufWriter::new(std::fs::File::create(&path)?);
            let mut written = 0u64;
            if head_insert {
                // 4 KiB of new bytes: every downstream byte shifts.
                let mut rng = Rng::new(seed ^ 0xDEAD);
                let mut head = vec![0u8; 4096];
                for chunk in head.chunks_mut(8) {
                    let v = rng.next().to_le_bytes();
                    chunk.copy_from_slice(&v[..chunk.len()]);
                }
                file.write_all(&head)?;
                written += head.len() as u64;
            }
            let order: Box<dyn Iterator<Item = u64>> = if reorder {
                Box::new((0..blocks).map(|i| {
                    let group = i / REORDER_GROUP;
                    let within = i % REORDER_GROUP;
                    let half = REORDER_GROUP / 2;
                    let base = group * REORDER_GROUP;
                    // Swap the group's halves; the tail group keeps its order.
                    if base + REORDER_GROUP <= blocks {
                        base + if within < half {
                            within + half
                        } else {
                            within - half
                        }
                    } else {
                        i
                    }
                }))
            } else {
                Box::new(0..blocks)
            };
            for i in order {
                let salt = if changed.contains(&i) { 1 } else { 0 };
                let block = block_bytes(seed, salt, i);
                file.write_all(&block)?;
                written += block.len() as u64;
            }
            file.flush()?;
            eprintln!("[gen] {name}: {}", human_bytes(written));
            Ok(written)
        };

    let none = HashSet::new();
    write_variant("v1.bin", &none, false, false)?;
    write_variant("v2-small.bin", &changed_set(3), false, false)?;
    write_variant("v2-medium.bin", &changed_set(15), false, false)?;
    write_variant("v2-large.bin", &changed_set(50), false, false)?;
    write_variant("v2-shifted.bin", &none, true, false)?;
    write_variant("v2-reordered.bin", &none, false, true)?;

    std::fs::write(
        out.join("dataset.json"),
        format!(
            "{{\"seed\":{seed},\"block_bytes\":{BLOCK},\"blocks\":{blocks},\"base_bytes\":{}}}\n",
            blocks * BLOCK as u64
        ),
    )?;
    println!(
        "dataset : {} — v1 + 5 update variants ({} blocks of 64 KiB)",
        out.display(),
        blocks
    );
    Ok(())
}

struct VersionReport {
    name: String,
    input_bytes: u64,
    pack_ms: u128,
    cavs_bytes: u64,
    unique_chunks: usize,
    manifest_json: usize,
    manifest_v2: usize,
    // Update metrics vs v1 (zero for v1 itself).
    new_chunks: usize,
    update_egress: u64,
}

pub fn suite(dataset: &Path, out: &Path) -> Result<()> {
    std::fs::create_dir_all(out)?;
    let versions = [
        "v1.bin",
        "v2-small.bin",
        "v2-medium.bin",
        "v2-large.bin",
        "v2-shifted.bin",
        "v2-reordered.bin",
    ];
    let opts = PackOptions {
        segment_time: 4.0,
        mode: Some(ChunkModeArg::Cdc),
        chunk_size: Some(BLOCK),
        profile: None,
        prev: None,
        bootstrap: false,
        compress: true,
        zstd_level: 3,
        force_transcode: false,
        sign_key: None,
        against_signature: None,
    };

    let mut base_chunks: Option<HashSet<[u8; 32]>> = None;
    let mut reports: Vec<VersionReport> = Vec::new();
    for name in versions {
        let input = dataset.join(name);
        if !input.is_file() {
            bail!(
                "{} not found — generate the dataset with `cavs bench gen`",
                input.display()
            );
        }
        let input_bytes = std::fs::metadata(&input)?.len();
        let cavs = out.join(format!("{name}.cavs"));
        let started = std::time::Instant::now();
        pack::pack_raw(std::slice::from_ref(&input), &cavs, &opts)
            .with_context(|| format!("packing {name}"))?;
        let pack_ms = started.elapsed().as_millis();

        let reader = cavs_format::Reader::open(&cavs)?;
        let manifest = cavs_manifest::manifest_from_reader(&reader, name)?;
        let manifest_json = serde_json::to_vec(&manifest)?.len();
        let manifest_v2 = cavs_manifest::encode_manifest_v2(&manifest)?.len();
        let chunks: Vec<_> = reader.chunks().to_vec();
        let hashes: HashSet<[u8; 32]> = chunks.iter().map(|c| c.hash).collect();
        let (new_chunks, update_egress) = match &base_chunks {
            None => (0, 0),
            Some(base) => {
                let new: Vec<_> = chunks.iter().filter(|c| !base.contains(&c.hash)).collect();
                (
                    new.len(),
                    new.iter().map(|c| c.len_stored as u64).sum::<u64>(),
                )
            }
        };
        if base_chunks.is_none() {
            base_chunks = Some(hashes);
        }
        eprintln!(
            "[suite] {name}: packed in {pack_ms} ms, {} unique chunks, update egress {}",
            chunks.len(),
            human_bytes(update_egress)
        );
        reports.push(VersionReport {
            name: name.to_string(),
            input_bytes,
            pack_ms,
            cavs_bytes: std::fs::metadata(&cavs)?.len(),
            unique_chunks: chunks.len(),
            manifest_json,
            manifest_v2,
            new_chunks,
            update_egress,
        });
    }

    // Physical shape check: ingest v1 + the small update into a fresh
    // packfile store (the operational layout) and count objects.
    let store_dir = out.join("packstore");
    if store_dir.exists() {
        std::fs::remove_dir_all(&store_dir)?;
    }
    crate::store::add(
        &store_dir,
        "v1",
        &out.join("v1.bin.cavs"),
        Some(crate::StorageArg::Packfiles),
    )?;
    crate::store::add(&store_dir, "v2-small", &out.join("v2-small.bin.cavs"), None)?;
    let mut packs = 0u64;
    let mut store_bytes = 0u64;
    for entry in walk_files(&store_dir)? {
        store_bytes += std::fs::metadata(&entry)?.len();
        if entry.extension().is_some_and(|e| e == "cavspack") {
            packs += 1;
        }
    }

    write_summaries(out, &reports, packs, store_bytes)?;
    println!("results : {}/summary.md + summary.json", out.display());
    Ok(())
}

fn walk_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    Ok(out)
}

fn write_summaries(
    out: &Path,
    reports: &[VersionReport],
    packs: u64,
    store_bytes: u64,
) -> Result<()> {
    let mut md = String::from(
        "# CAVS synthetic build benchmark\n\n\
         FastCDC 64 KiB + zstd 3, deterministic dataset (`cavs bench gen`).\n\n\
         | Version | Input | Pack | .cavs | Unique chunks | Manifest v1/v2 | Update egress |\n\
         |---|---:|---:|---:|---:|---|---:|\n",
    );
    let mut json = String::from("{\"versions\":[");
    for (i, r) in reports.iter().enumerate() {
        let update = if i == 0 {
            "—".to_string()
        } else {
            format!(
                "{} ({} new chunks, {:.1}%)",
                human_bytes(r.update_egress),
                r.new_chunks,
                r.update_egress as f64 * 100.0 / r.input_bytes.max(1) as f64
            )
        };
        md.push_str(&format!(
            "| {} | {} | {} ms | {} | {} | {} / {} | {} |\n",
            r.name,
            human_bytes(r.input_bytes),
            r.pack_ms,
            human_bytes(r.cavs_bytes),
            r.unique_chunks,
            human_bytes(r.manifest_json as u64),
            human_bytes(r.manifest_v2 as u64),
            update,
        ));
        if i > 0 {
            json.push(',');
        }
        json.push_str(&format!(
            "{{\"name\":\"{}\",\"input_bytes\":{},\"pack_ms\":{},\"cavs_bytes\":{},\"unique_chunks\":{},\"manifest_json_bytes\":{},\"manifest_v2_bytes\":{},\"update_new_chunks\":{},\"update_egress_bytes\":{}}}",
            r.name,
            r.input_bytes,
            r.pack_ms,
            r.cavs_bytes,
            r.unique_chunks,
            r.manifest_json,
            r.manifest_v2,
            r.new_chunks,
            r.update_egress,
        ));
    }
    md.push_str(&format!(
        "\nPackfile store (v1 + v2-small ingested): **{packs} packfiles**, {} on disk.\n",
        human_bytes(store_bytes)
    ));
    json.push_str(&format!(
        "],\"packstore\":{{\"packfiles\":{packs},\"disk_bytes\":{store_bytes}}}}}\n"
    ));
    std::fs::write(out.join("summary.md"), md)?;
    std::fs::write(out.join("summary.json"), json)?;
    Ok(())
}

/// Deterministic synthetic *directory* builds (v0.7.0): `Build_v1/` plus a
/// `Build_v2/` with the update shapes that matter for per-file delivery —
/// one big artifact with a 3% block change, an edited catalog, added,
/// deleted and renamed assets, and untouched files that must no-op.
pub fn generate_dir(out: &Path, size: &str, seed: u64) -> Result<()> {
    let total = parse_size(size)?;
    let v1 = out.join("Build_v1");
    let v2 = out.join("Build_v2");
    for d in [&v1, &v2] {
        if d.exists() {
            std::fs::remove_dir_all(d)?;
        }
        std::fs::create_dir_all(d.join("assets"))?;
        std::fs::create_dir_all(d.join("bin"))?;
        std::fs::create_dir_all(d.join("data"))?;
    }

    // Layout: 60% one PCK-like artifact, 5% engine binary, ~2% textual
    // catalog, the rest split across 40 small assets.
    let pck_blocks = (total * 60 / 100).div_ceil(BLOCK as u64).max(2);
    let bin_blocks = (total * 5 / 100).div_ceil(BLOCK as u64).max(1);
    let asset_blocks = ((total * 33 / 100) / 40).div_ceil(BLOCK as u64).max(1);

    let write_blocks =
        |path: &Path, blocks: u64, salt_of: &dyn Fn(u64) -> u64, tag: u64| -> Result<()> {
            let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
            for i in 0..blocks {
                file.write_all(&block_bytes(seed ^ tag, salt_of(i), i))?;
            }
            file.flush()?;
            Ok(())
        };

    // content.pck: v2 changes ~3% of blocks.
    let changed: HashSet<u64> = {
        let mut rng = Rng::new(seed.wrapping_mul(97).wrapping_add(3));
        let target = (pck_blocks * 3 / 100).max(1);
        let mut set = HashSet::new();
        while (set.len() as u64) < target {
            set.insert(rng.next() % pck_blocks);
        }
        set
    };
    write_blocks(&v1.join("content.pck"), pck_blocks, &|_| 0, 1)?;
    write_blocks(
        &v2.join("content.pck"),
        pck_blocks,
        &|i| if changed.contains(&i) { 1 } else { 0 },
        1,
    )?;

    // Engine binary: identical in both versions (must no-op), executable.
    write_blocks(&v1.join("bin/app"), bin_blocks, &|_| 0, 2)?;
    write_blocks(&v2.join("bin/app"), bin_blocks, &|_| 0, 2)?;
    #[cfg(unix)]
    for d in [&v1, &v2] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(d.join("bin/app"), std::fs::Permissions::from_mode(0o755))?;
    }

    // Textual catalog: small edit in v2.
    let catalog_v1: String = (0..2000)
        .map(|i| format!("asset_{i:04} = {{ id = {i}, price = {} }}\n", i * 7 % 991))
        .collect();
    std::fs::write(v1.join("data/catalog.json"), &catalog_v1)?;
    let catalog_v2 = catalog_v1.replace("price = 700", "price = 350")
        + "asset_2000 = { id = 2000, price = 42 }\n";
    std::fs::write(v2.join("data/catalog.json"), catalog_v2)?;

    // 40 small assets: v2 adds 2, deletes 1, renames 1, edits 1.
    for n in 0..40u64 {
        let name = format!("assets/asset_{n:02}.dat");
        write_blocks(&v1.join(&name), asset_blocks, &|_| 0, 100 + n)?;
        match n {
            // deleted in v2
            7 => {}
            // renamed in v2 (same bytes, new name)
            13 => write_blocks(
                &v2.join("assets/asset_13_renamed.dat"),
                asset_blocks,
                &|_| 0,
                100 + n,
            )?,
            // edited in v2
            21 => write_blocks(&v2.join(&name), asset_blocks, &|_| 1, 100 + n)?,
            _ => write_blocks(&v2.join(&name), asset_blocks, &|_| 0, 100 + n)?,
        }
    }
    for n in 40..42u64 {
        let name = format!("assets/asset_{n:02}.dat");
        write_blocks(&v2.join(&name), asset_blocks, &|_| 0, 100 + n)?;
    }

    println!(
        "dataset : {} — Build_v1 ({}) and Build_v2 with modified/new/deleted/renamed files",
        out.display(),
        human_bytes(dir_size(&v1)?),
    );
    Ok(())
}

fn dir_size(root: &Path) -> Result<u64> {
    Ok(walk_files(root)?
        .iter()
        .map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .sum())
}

/// Public alias used by other bench modules.
pub(crate) fn parse_size_pub(s: &str) -> Result<u64> {
    parse_size(s)
}

/// Parse a human size: plain bytes or 1024-based KiB/MiB/GiB/TiB suffixes.
fn parse_size(s: &str) -> Result<u64> {
    let t = s.trim();
    let split = t
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(t.len());
    let (num, suffix) = t.split_at(split);
    let value: f64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("cannot parse size {s:?}"))?;
    let mult: u64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        other => bail!("unknown size suffix {other:?} in {s:?}"),
    };
    Ok((value * mult as f64) as u64)
}
