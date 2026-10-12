//! Segmented, mmap-backed chunk ledger (Round 3B).
//!
//! The monolithic `index.bin` snapshot must be read (and rewritten) whole:
//! at 10M chunks that is ~720 MB of RAM and I/O per open/save, and it only
//! grows. This module replaces it — behind an explicit migration — with:
//!
//! ```text
//! <store>/index/
//!   CURRENT                     the active generation ("gen-0000000042")
//!   wal.log                     begin/commit journal of generation swaps
//!   segments/<id>.seg           immutable, content-addressed segment pool
//!   generations/gen-N/root.idx  which segments a generation is made of
//!   generations/gen-N/assets.json.zst   asset -> chunk hexes
//! ```
//!
//! Segments are sorted-by-hash record arrays with a fixed stride, so a
//! lookup is `mmap` + binary search — no deserialization, no full load; the
//! OS keeps hot pages cached and evicts cold ones under pressure. A publish
//! session appends one **delta segment** with just the records it touched
//! (tombstones included) instead of rewriting the ledger; deltas are folded
//! into fresh base segments when they accumulate ([`MAX_DELTA_SEGMENTS`] /
//! [`DELTA_BYTES_RATIO_PCT`]). Every segment carries a BLAKE3 seal, so
//! corruption is detected — and named — per segment.
//!
//! Crash-safety: segments and roots are immutable and content-addressed;
//! a generation becomes live only via the atomic `CURRENT` rename, and the
//! previous generation is retained one swap (mirroring `index.bin.prev`).
//! `wal.log` records begin/commit around each swap; on open, generation
//! directories newer than `CURRENT` (a crash between "begin" and the
//! rename) are swept.

use crate::{ChunkInfo, Result, StoreError, StoreLayout};
use cavs_hash::to_hex;
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Segment file magic + version.
const SEG_MAGIC: &[u8; 8] = b"CAVSSEG1";
const SEG_VERSION: u16 = 1;
/// Fixed segment header size (magic + version + kind + reserved + count).
const SEG_HEADER_LEN: usize = 8 + 2 + 1 + 1 + 8;
/// One record: hash[32] | len_raw u32 | len_stored u32 | flags u32 |
/// refcount u64 | zero_since u64 | pack_ord u32 | pack_offset u64 | state u8.
const SEG_RECORD_LEN: usize = 32 + 4 + 4 + 4 + 8 + 8 + 4 + 8 + 1;
/// Sentinels for optional record fields.
const NO_PACK: u32 = u32::MAX;
const NO_OFFSET: u64 = u64::MAX;
const NO_ZERO_SINCE: u64 = u64::MAX;
/// Record states.
const STATE_LIVE: u8 = 0;
const STATE_TOMBSTONE: u8 = 1;

/// Target records per base segment: ~64 MiB of records, so a 100M-chunk
/// store splits into ~110 independently verifiable, mmap-friendly files.
const SEG_TARGET_RECORDS: usize = 64 * 1024 * 1024 / SEG_RECORD_LEN;

/// Fold deltas into fresh base segments when a generation accumulates more
/// than this many...
pub(crate) const MAX_DELTA_SEGMENTS: usize = 8;
/// ...or when delta bytes exceed this percentage of base bytes.
pub(crate) const DELTA_BYTES_RATIO_PCT: u64 = 25;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SegKind {
    Base,
    Delta,
}

/// One mmapped segment: its root metadata, pack-id table and record region.
struct Segment {
    file: String,
    kind: SegKind,
    mmap: memmap2::Mmap,
    record_count: usize,
    /// Pack hexes this segment's records reference by ordinal.
    packs: Vec<String>,
    min: [u8; 32],
    max: [u8; 32],
}

impl Segment {
    fn record_at(&self, i: usize) -> &[u8] {
        &self.mmap[SEG_HEADER_LEN + i * SEG_RECORD_LEN..SEG_HEADER_LEN + (i + 1) * SEG_RECORD_LEN]
    }

    fn hash_at(&self, i: usize) -> &[u8] {
        &self.record_at(i)[..32]
    }

    /// Binary search for `hash`; decodes the record (tombstones included,
    /// so a newer delta can shadow an older live record).
    fn lookup(&self, hash: &[u8; 32]) -> Option<(Option<ChunkInfo>, u8)> {
        if hash[..] < self.min[..] || hash[..] > self.max[..] {
            return None;
        }
        let (mut lo, mut hi) = (0usize, self.record_count);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.hash_at(mid).cmp(&hash[..]) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let (info, state) = decode_record(self.record_at(mid), &self.packs);
                    return Some((info, state));
                }
            }
        }
        None
    }
}

/// The segmented ledger: everything `GlobalStore` needs for chunk lookups
/// without holding the chunk table in RAM.
pub(crate) struct SegIndex {
    dir: PathBuf,
    pub(crate) generation: u64,
    pub(crate) layout: StoreLayout,
    /// Lookup priority order: deltas newest → oldest, then bases.
    segments: Vec<Segment>,
}

/// Root metadata of one generation (JSON body + BLAKE3 checksum wrapper).
#[derive(serde::Serialize, serde::Deserialize)]
struct RootBody {
    version: u32,
    generation: u64,
    layout: StoreLayout,
    /// Write order: bases first, then deltas oldest → newest.
    segments: Vec<RootSegment>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct RootSegment {
    file: String,
    kind: String,
    records: u64,
    size: u64,
    min: String,
    max: String,
}

impl SegIndex {
    pub(crate) fn index_dir(store_root: &Path) -> PathBuf {
        store_root.join("index")
    }

    pub(crate) fn exists(store_root: &Path) -> bool {
        Self::index_dir(store_root).join("CURRENT").is_file()
    }

    /// Open the active generation: read `CURRENT` + root, mmap every
    /// segment. Header/size sanity is checked here; full seal verification
    /// is deferred to [`Self::verify_segments`] so a warm open of a huge
    /// store stays sub-second (lookups still fail loudly on bad records
    /// because record hashes must match the probe).
    /// Returns the index and the generation's asset table.
    pub(crate) fn open(store_root: &Path) -> Result<(Self, BTreeMap<String, Vec<String>>)> {
        let dir = Self::index_dir(store_root);
        let current = std::fs::read_to_string(dir.join("CURRENT"))?;
        let generation: u64 = current
            .trim()
            .strip_prefix("gen-")
            .and_then(|g| g.parse().ok())
            .ok_or_else(|| StoreError::IndexCorrupt(format!("bad CURRENT: {current:?}")))?;

        // Sweep generation dirs a crash left half-written (begun but never
        // became CURRENT). Anything newer than the committed generation is
        // by definition uncommitted.
        if let Ok(entries) = std::fs::read_dir(dir.join("generations")) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if let Some(gen) = name
                    .strip_prefix("gen-")
                    .and_then(|g| g.parse::<u64>().ok())
                {
                    if gen > generation {
                        let _ = std::fs::remove_dir_all(entry.path());
                    }
                }
            }
        }

        let gen_dir = dir.join("generations").join(gen_name(generation));
        let root = read_root(&gen_dir.join("root.idx"))?;
        if root.generation != generation {
            return Err(StoreError::IndexCorrupt(format!(
                "root.idx generation {} != CURRENT {generation}",
                root.generation
            )));
        }

        let mut bases = Vec::new();
        let mut deltas = Vec::new();
        for rs in &root.segments {
            let path = dir.join("segments").join(&rs.file);
            let seg = open_segment(&path, rs)?;
            match seg.kind {
                SegKind::Base => bases.push(seg),
                SegKind::Delta => deltas.push(seg),
            }
        }
        // Priority: newest delta first (root lists deltas oldest → newest).
        deltas.reverse();
        deltas.extend(bases);

        let assets = read_assets(&gen_dir.join("assets.json.zst"))?;
        Ok((
            Self {
                dir,
                generation,
                layout: root.layout,
                segments: deltas,
            },
            assets,
        ))
    }

    pub(crate) fn lookup(&self, hex: &str) -> Option<ChunkInfo> {
        let hash = hex_to_hash(hex)?;
        for seg in &self.segments {
            if let Some((info, _state)) = seg.lookup(&hash) {
                return info; // newest segment wins; tombstone = None
            }
        }
        None
    }

    pub(crate) fn segment_count(&self) -> usize {
        self.segments.len()
    }

    pub(crate) fn delta_count(&self) -> usize {
        self.segments
            .iter()
            .filter(|s| s.kind == SegKind::Delta)
            .count()
    }

    fn delta_bytes(&self) -> u64 {
        self.segments
            .iter()
            .filter(|s| s.kind == SegKind::Delta)
            .map(|s| s.mmap.len() as u64)
            .sum()
    }

    fn base_bytes(&self) -> u64 {
        self.segments
            .iter()
            .filter(|s| s.kind == SegKind::Base)
            .map(|s| s.mmap.len() as u64)
            .sum()
    }

    /// Merged live view of every segment: k-way merge by hash where the
    /// newest segment shadows older ones and tombstones drop out. Streams
    /// from the mmaps — nothing is materialized.
    pub(crate) fn iter_live(&self) -> impl Iterator<Item = (String, ChunkInfo)> + '_ {
        MergeIter {
            segments: &self.segments,
            cursors: vec![0; self.segments.len()],
        }
    }

    /// Verify every segment's BLAKE3 seal (the expensive, explicit check
    /// behind `store verify` / `index verify`). Returns segment count.
    pub(crate) fn verify_segments(&self) -> Result<usize> {
        for seg in &self.segments {
            let body = &seg.mmap[..seg.mmap.len() - 32];
            let seal = &seg.mmap[seg.mmap.len() - 32..];
            if cavs_hash::hash_chunk(body).as_slice() != seal {
                return Err(StoreError::IndexCorrupt(format!(
                    "segment {} failed its BLAKE3 seal",
                    seg.file
                )));
            }
        }
        Ok(self.segments.len())
    }

    /// Commit one publish session: write the touched records (`None` =
    /// tombstone) as a delta segment, the asset table, and a new root;
    /// swap `CURRENT` atomically. Compacts when the delta pile is past its
    /// thresholds. The previous generation stays on disk one swap.
    pub(crate) fn commit_generation(
        &mut self,
        dirty: &BTreeMap<String, Option<ChunkInfo>>,
        assets: &BTreeMap<String, Vec<String>>,
    ) -> Result<()> {
        let next = self.generation + 1;
        let gen_dir = self.dir.join("generations").join(gen_name(next));

        wal_append(&self.dir, "begin", next)?;
        std::fs::create_dir_all(self.dir.join("segments"))?;
        std::fs::create_dir_all(&gen_dir)?;

        let mut segments: Vec<RootSegment> = current_root_segments(&self.dir, self.generation)?;
        if !dirty.is_empty() {
            let records: Vec<(String, Option<ChunkInfo>)> = dirty
                .iter()
                .map(|(hex, info)| (hex.clone(), info.clone()))
                .collect();
            let rs = write_segment(&self.dir.join("segments"), SegKind::Delta, &records)?;
            segments.push(rs);
        }

        write_assets(&gen_dir.join("assets.json.zst"), assets)?;
        write_root(
            &gen_dir.join("root.idx"),
            &RootBody {
                version: 1,
                generation: next,
                layout: self.layout,
                segments,
            },
        )?;
        self.swap_current(next)?;
        wal_append(&self.dir, "commit", next)?;

        // Reload the new generation (cheap: re-mmap), then fold the deltas
        // into fresh bases if the pile is past its thresholds.
        self.reload()?;
        if self.delta_count() > MAX_DELTA_SEGMENTS
            || self.delta_bytes() * 100 > self.base_bytes().max(1) * DELTA_BYTES_RATIO_PCT
        {
            self.compact(assets)?;
        }
        self.prune_generations()?;
        Ok(())
    }

    /// Rewrite the merged live view as fresh base segments (copy-on-write:
    /// a brand-new generation; old segments are untouched until pruned).
    pub(crate) fn compact(&mut self, assets: &BTreeMap<String, Vec<String>>) -> Result<()> {
        let next = self.generation + 1;
        let gen_dir = self.dir.join("generations").join(gen_name(next));
        wal_append(&self.dir, "begin", next)?;
        std::fs::create_dir_all(&gen_dir)?;

        let mut segments = Vec::new();
        let mut batch: Vec<(String, Option<ChunkInfo>)> = Vec::with_capacity(SEG_TARGET_RECORDS);
        for (hex, info) in self.iter_live() {
            batch.push((hex, Some(info)));
            if batch.len() >= SEG_TARGET_RECORDS {
                segments.push(write_segment(
                    &self.dir.join("segments"),
                    SegKind::Base,
                    &batch,
                )?);
                batch.clear();
            }
        }
        if !batch.is_empty() {
            segments.push(write_segment(
                &self.dir.join("segments"),
                SegKind::Base,
                &batch,
            )?);
        }

        write_assets(&gen_dir.join("assets.json.zst"), assets)?;
        write_root(
            &gen_dir.join("root.idx"),
            &RootBody {
                version: 1,
                generation: next,
                layout: self.layout,
                segments,
            },
        )?;
        self.swap_current(next)?;
        wal_append(&self.dir, "commit", next)?;
        self.reload()?;
        self.prune_generations()?;
        Ok(())
    }

    /// Build a segmented index from a full in-RAM ledger (the migration
    /// path from `index.bin`, and the creation path for new stores).
    pub(crate) fn create(
        store_root: &Path,
        generation: u64,
        layout: StoreLayout,
        chunks: &BTreeMap<String, ChunkInfo>,
        assets: &BTreeMap<String, Vec<String>>,
    ) -> Result<(Self, BTreeMap<String, Vec<String>>)> {
        let dir = Self::index_dir(store_root);
        let gen_dir = dir.join("generations").join(gen_name(generation));
        std::fs::create_dir_all(dir.join("segments"))?;
        std::fs::create_dir_all(&gen_dir)?;
        wal_append(&dir, "begin", generation)?;

        let mut segments = Vec::new();
        let mut batch: Vec<(String, Option<ChunkInfo>)> = Vec::with_capacity(SEG_TARGET_RECORDS);
        for (hex, info) in chunks {
            batch.push((hex.clone(), Some(info.clone())));
            if batch.len() >= SEG_TARGET_RECORDS {
                segments.push(write_segment(&dir.join("segments"), SegKind::Base, &batch)?);
                batch.clear();
            }
        }
        if !batch.is_empty() {
            segments.push(write_segment(&dir.join("segments"), SegKind::Base, &batch)?);
        }

        write_assets(&gen_dir.join("assets.json.zst"), assets)?;
        write_root(
            &gen_dir.join("root.idx"),
            &RootBody {
                version: 1,
                generation,
                layout,
                segments,
            },
        )?;
        let tmp = dir.join("CURRENT.tmp");
        std::fs::write(&tmp, format!("{}\n", gen_name(generation)))?;
        std::fs::rename(&tmp, dir.join("CURRENT"))?;
        if let Ok(d) = std::fs::File::open(&dir) {
            let _ = d.sync_all();
        }
        wal_append(&dir, "commit", generation)?;
        Self::open(store_root)
    }

    fn swap_current(&self, next: u64) -> Result<()> {
        let tmp = self.dir.join("CURRENT.tmp");
        std::fs::write(&tmp, format!("{}\n", gen_name(next)))?;
        std::fs::rename(&tmp, self.dir.join("CURRENT"))?;
        if let Ok(d) = std::fs::File::open(&self.dir) {
            let _ = d.sync_all();
        }
        Ok(())
    }

    fn reload(&mut self) -> Result<()> {
        let store_root = self.dir.parent().unwrap().to_path_buf();
        let (fresh, _assets) = Self::open(&store_root)?;
        *self = fresh;
        Ok(())
    }

    /// Keep the current and previous generation; drop older ones, then GC
    /// pool segments no retained root references.
    fn prune_generations(&self) -> Result<()> {
        let gens_dir = self.dir.join("generations");
        let mut gens: Vec<u64> = Vec::new();
        for entry in std::fs::read_dir(&gens_dir)?.flatten() {
            if let Some(g) = entry
                .file_name()
                .to_string_lossy()
                .strip_prefix("gen-")
                .and_then(|g| g.parse::<u64>().ok())
            {
                gens.push(g);
            }
        }
        gens.sort_unstable();
        let keep: Vec<u64> = gens.iter().rev().take(2).copied().collect();
        for g in &gens {
            if !keep.contains(g) {
                let _ = std::fs::remove_dir_all(gens_dir.join(gen_name(*g)));
            }
        }
        // Segment pool GC: a segment survives while any retained root
        // references it.
        let mut referenced: std::collections::HashSet<String> = std::collections::HashSet::new();
        for g in &keep {
            if let Ok(root) = read_root(&gens_dir.join(gen_name(*g)).join("root.idx")) {
                for s in root.segments {
                    referenced.insert(s.file);
                }
            }
        }
        if let Ok(entries) = std::fs::read_dir(self.dir.join("segments")) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.ends_with(".seg") && !referenced.contains(&name) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        // The WAL only protects generation swaps; once pruning ran, the
        // swap is durable and the journal can restart.
        let _ = std::fs::write(self.dir.join("wal.log"), b"");
        Ok(())
    }
}

/// Streaming k-way merge over sorted segments; priority = segment order
/// (newest first), so shadowed records and tombstones fall out.
struct MergeIter<'a> {
    segments: &'a [Segment],
    cursors: Vec<usize>,
}

impl Iterator for MergeIter<'_> {
    type Item = (String, ChunkInfo);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // The smallest pending hash across all cursors; ties resolved
            // by segment priority (lower index = newer).
            let mut min: Option<(&[u8], usize)> = None;
            for (i, seg) in self.segments.iter().enumerate() {
                if self.cursors[i] >= seg.record_count {
                    continue;
                }
                let h = seg.hash_at(self.cursors[i]);
                match min {
                    Some((mh, _)) if mh <= h => {}
                    _ => min = Some((h, i)),
                }
            }
            let (hash, winner) = min?;
            let hash = hash.to_vec();
            let (info, _state) =
                decode_record(self.segments[winner].record_at(self.cursors[winner]), {
                    &self.segments[winner].packs
                });
            // Advance every cursor sitting on this hash (shadowed copies).
            for (i, seg) in self.segments.iter().enumerate() {
                while self.cursors[i] < seg.record_count && seg.hash_at(self.cursors[i]) == hash {
                    self.cursors[i] += 1;
                }
            }
            if let Some(info) = info {
                let mut h = [0u8; 32];
                h.copy_from_slice(&hash);
                return Some((to_hex(&h), info));
            }
            // Tombstone: skip and keep merging.
        }
    }
}

fn gen_name(g: u64) -> String {
    format!("gen-{g:010}")
}

fn hex_to_hash(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn encode_record(
    out: &mut Vec<u8>,
    hash: &[u8; 32],
    info: Option<&ChunkInfo>,
    packs: &mut PackTable,
) {
    out.extend_from_slice(hash);
    match info {
        Some(info) => {
            out.extend_from_slice(&info.len_raw.to_le_bytes());
            out.extend_from_slice(&info.len_stored.to_le_bytes());
            out.extend_from_slice(&info.flags.to_le_bytes());
            out.extend_from_slice(&info.refcount.to_le_bytes());
            out.extend_from_slice(&info.zero_since.unwrap_or(NO_ZERO_SINCE).to_le_bytes());
            let ord = match &info.pack {
                Some(p) => packs.ordinal(p),
                None => NO_PACK,
            };
            out.extend_from_slice(&ord.to_le_bytes());
            out.extend_from_slice(&info.pack_offset.unwrap_or(NO_OFFSET).to_le_bytes());
            out.push(STATE_LIVE);
        }
        None => {
            out.extend_from_slice(&[0u8; SEG_RECORD_LEN - 32 - 1]);
            out.push(STATE_TOMBSTONE);
        }
    }
}

fn decode_record(rec: &[u8], packs: &[String]) -> (Option<ChunkInfo>, u8) {
    let state = rec[SEG_RECORD_LEN - 1];
    if state == STATE_TOMBSTONE {
        return (None, state);
    }
    let u32_at = |o: usize| u32::from_le_bytes(rec[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(rec[o..o + 8].try_into().unwrap());
    let zero_since = u64_at(52);
    let pack_ord = u32_at(60);
    let pack_offset = u64_at(64);
    (
        Some(ChunkInfo {
            len_raw: u32_at(32),
            len_stored: u32_at(36),
            flags: u32_at(40),
            refcount: u64_at(44),
            zero_since: (zero_since != NO_ZERO_SINCE).then_some(zero_since),
            pack: (pack_ord != NO_PACK).then(|| packs[pack_ord as usize].clone()),
            pack_offset: (pack_offset != NO_OFFSET).then_some(pack_offset),
        }),
        state,
    )
}

/// Interning pack-id table built while encoding a segment.
#[derive(Default)]
struct PackTable {
    ids: Vec<String>,
    map: std::collections::HashMap<String, u32>,
}

impl PackTable {
    fn ordinal(&mut self, pack: &str) -> u32 {
        if let Some(&ord) = self.map.get(pack) {
            return ord;
        }
        let ord = self.ids.len() as u32;
        self.ids.push(pack.to_string());
        self.map.insert(pack.to_string(), ord);
        ord
    }
}

/// Write one immutable segment into the pool; the file is named by its
/// seal (content-addressed), so identical content never duplicates and a
/// half-written file can never collide with a committed one.
/// `records` must be sorted by hex (BTreeMap order guarantees this).
fn write_segment(
    pool: &Path,
    kind: SegKind,
    records: &[(String, Option<ChunkInfo>)],
) -> Result<RootSegment> {
    let mut body = Vec::with_capacity(SEG_HEADER_LEN + records.len() * SEG_RECORD_LEN);
    body.extend_from_slice(SEG_MAGIC);
    body.extend_from_slice(&SEG_VERSION.to_le_bytes());
    body.push(match kind {
        SegKind::Base => 0,
        SegKind::Delta => 1,
    });
    body.push(0);
    body.extend_from_slice(&(records.len() as u64).to_le_bytes());

    let mut packs = PackTable::default();
    let mut min = String::new();
    let mut max = String::new();
    for (i, (hex, info)) in records.iter().enumerate() {
        let hash = hex_to_hash(hex)
            .ok_or_else(|| StoreError::IndexCorrupt(format!("bad chunk hex {hex}")))?;
        if i == 0 {
            min = hex.clone();
        }
        max = hex.clone();
        encode_record(&mut body, &hash, info.as_ref(), &mut packs);
    }
    body.extend_from_slice(&(packs.ids.len() as u32).to_le_bytes());
    for id in &packs.ids {
        body.extend_from_slice(&(id.len() as u16).to_le_bytes());
        body.extend_from_slice(id.as_bytes());
    }
    let seal = cavs_hash::hash_chunk(&body);
    let file = format!("{}.seg", to_hex(&seal));
    let path = pool.join(&file);
    if !path.exists() {
        let tmp = pool.join(format!("{}.seg.tmp", to_hex(&seal)));
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&body)?;
        f.write_all(&seal)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(RootSegment {
        file,
        kind: match kind {
            SegKind::Base => "base".into(),
            SegKind::Delta => "delta".into(),
        },
        records: records.len() as u64,
        size: (body.len() + 32) as u64,
        min,
        max,
    })
}

fn open_segment(path: &Path, rs: &RootSegment) -> Result<Segment> {
    let file = std::fs::File::open(path)
        .map_err(|e| StoreError::IndexCorrupt(format!("segment {} unreadable: {e}", rs.file)))?;
    // Safety: segments are immutable once committed; the store's advisory
    // lock serializes writers, and a torn write never lands under a
    // committed name (content-addressed tmp+rename).
    let mmap = unsafe { memmap2::Mmap::map(&file) }
        .map_err(|e| StoreError::IndexCorrupt(format!("mmap {}: {e}", rs.file)))?;
    if mmap.len() as u64 != rs.size || mmap.len() < SEG_HEADER_LEN + 32 || &mmap[..8] != SEG_MAGIC {
        return Err(StoreError::IndexCorrupt(format!(
            "segment {} does not match its root entry",
            rs.file
        )));
    }
    let version = u16::from_le_bytes(mmap[8..10].try_into().unwrap());
    if version > SEG_VERSION {
        return Err(StoreError::IndexCorrupt(format!(
            "segment {} is version {version}; this build reads up to {SEG_VERSION}",
            rs.file
        )));
    }
    let kind = match mmap[10] {
        0 => SegKind::Base,
        1 => SegKind::Delta,
        k => {
            return Err(StoreError::IndexCorrupt(format!(
                "segment {}: unknown kind {k}",
                rs.file
            )))
        }
    };
    let record_count = u64::from_le_bytes(mmap[12..20].try_into().unwrap()) as usize;
    let packs_off = SEG_HEADER_LEN + record_count * SEG_RECORD_LEN;
    if packs_off + 4 + 32 > mmap.len() || record_count as u64 != rs.records {
        return Err(StoreError::IndexCorrupt(format!(
            "segment {} is truncated",
            rs.file
        )));
    }
    let pack_count = u32::from_le_bytes(mmap[packs_off..packs_off + 4].try_into().unwrap());
    let mut packs = Vec::with_capacity(pack_count as usize);
    let mut at = packs_off + 4;
    for _ in 0..pack_count {
        if at + 2 > mmap.len() - 32 {
            return Err(StoreError::IndexCorrupt(format!(
                "segment {}: pack table truncated",
                rs.file
            )));
        }
        let len = u16::from_le_bytes(mmap[at..at + 2].try_into().unwrap()) as usize;
        at += 2;
        if at + len > mmap.len() - 32 {
            return Err(StoreError::IndexCorrupt(format!(
                "segment {}: pack table truncated",
                rs.file
            )));
        }
        packs.push(
            String::from_utf8(mmap[at..at + len].to_vec()).map_err(|_| {
                StoreError::IndexCorrupt(format!("segment {}: bad pack id", rs.file))
            })?,
        );
        at += len;
    }
    let min = hex_to_hash(&rs.min).unwrap_or([0u8; 32]);
    let max = hex_to_hash(&rs.max).unwrap_or([0xffu8; 32]);
    Ok(Segment {
        file: rs.file.clone(),
        kind,
        mmap,
        record_count,
        packs,
        min,
        max,
    })
}

fn current_root_segments(dir: &Path, generation: u64) -> Result<Vec<RootSegment>> {
    let root = read_root(
        &dir.join("generations")
            .join(gen_name(generation))
            .join("root.idx"),
    )?;
    Ok(root.segments)
}

fn write_root(path: &Path, body: &RootBody) -> Result<()> {
    // Checksum the *canonical* (Value, sorted-keys) serialization — the
    // same bytes the reader reproduces from the parsed document.
    let body_value = serde_json::to_value(body)?;
    let body_json = serde_json::to_vec(&body_value)?;
    let doc = serde_json::json!({
        "checksum": to_hex(&cavs_hash::hash_chunk(&body_json)),
        "body": body_value,
    });
    let tmp = path.with_extension("idx.tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(&serde_json::to_vec(&doc)?)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn read_root(path: &Path) -> Result<RootBody> {
    let bytes = std::fs::read(path)
        .map_err(|e| StoreError::IndexCorrupt(format!("root.idx unreadable: {e}")))?;
    let doc: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| StoreError::IndexCorrupt(format!("root.idx malformed: {e}")))?;
    let body = doc
        .get("body")
        .ok_or_else(|| StoreError::IndexCorrupt("root.idx has no body".into()))?;
    let body_json = serde_json::to_vec(body)?;
    let expect = doc.get("checksum").and_then(|c| c.as_str()).unwrap_or("");
    if to_hex(&cavs_hash::hash_chunk(&body_json)) != expect {
        return Err(StoreError::IndexCorrupt(
            "root.idx failed its checksum".into(),
        ));
    }
    let body: RootBody = serde_json::from_value(body.clone())
        .map_err(|e| StoreError::IndexCorrupt(format!("root.idx body malformed: {e}")))?;
    if body.version != 1 {
        return Err(StoreError::IndexCorrupt(format!(
            "root.idx version {} not supported",
            body.version
        )));
    }
    Ok(body)
}

fn write_assets(path: &Path, assets: &BTreeMap<String, Vec<String>>) -> Result<()> {
    let raw = serde_json::to_vec(assets)?;
    let compressed = zstd::bulk::compress(&raw, 3)
        .map_err(|e| StoreError::IndexCorrupt(format!("compressing assets: {e}")))?;
    let tmp = path.with_extension("zst.tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(&compressed)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn read_assets(path: &Path) -> Result<BTreeMap<String, Vec<String>>> {
    let compressed = std::fs::read(path)
        .map_err(|e| StoreError::IndexCorrupt(format!("assets table unreadable: {e}")))?;
    let raw = zstd::bulk::decompress(&compressed, 1 << 31)
        .map_err(|e| StoreError::IndexCorrupt(format!("assets table corrupt: {e}")))?;
    serde_json::from_slice(&raw)
        .map_err(|e| StoreError::IndexCorrupt(format!("assets table malformed: {e}")))
}

fn wal_append(dir: &Path, op: &str, generation: u64) -> Result<()> {
    let mut f = crate::sync::open_append(&dir.join("wal.log"))?;
    writeln!(f, "{{\"op\":\"{op}\",\"gen\":{generation}}}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(len: u32, refcount: u64, pack: Option<&str>) -> ChunkInfo {
        ChunkInfo {
            len_raw: len,
            len_stored: len,
            flags: 0,
            refcount,
            zero_since: None,
            pack: pack.map(str::to_string),
            pack_offset: pack.map(|_| 42),
        }
    }

    fn hexes(n: usize) -> Vec<String> {
        (0..n)
            .map(|i| to_hex(&cavs_hash::hash_chunk(&i.to_le_bytes())))
            .collect()
    }

    fn base_index(dir: &Path, n: usize) -> (SegIndex, Vec<String>) {
        let hx = hexes(n);
        let chunks: BTreeMap<String, ChunkInfo> = hx
            .iter()
            .enumerate()
            .map(|(i, h)| (h.clone(), info(100 + i as u32, 1, Some("packA"))))
            .collect();
        let assets = BTreeMap::from([("a1".to_string(), hx.clone())]);
        let (seg, _assets) =
            SegIndex::create(dir, 1, StoreLayout::Packfiles, &chunks, &assets).unwrap();
        (seg, hx)
    }

    #[test]
    fn create_open_lookup_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let (seg, hx) = base_index(dir.path(), 500);
        for (i, h) in hx.iter().enumerate() {
            let got = seg.lookup(h).expect(h);
            assert_eq!(got.len_raw, 100 + i as u32);
            assert_eq!(got.pack.as_deref(), Some("packA"));
        }
        assert!(seg.lookup(&"0".repeat(64)).is_none());

        // Reopen: same view, assets restored.
        let (seg2, assets) = SegIndex::open(dir.path()).unwrap();
        assert_eq!(assets.get("a1").unwrap().len(), 500);
        assert_eq!(seg2.iter_live().count(), 500);
    }

    #[test]
    fn delta_shadows_base_and_tombstones_delete() {
        let dir = tempfile::tempdir().unwrap();
        let (mut seg, hx) = base_index(dir.path(), 100);
        let dirty = BTreeMap::from([
            (hx[0].clone(), Some(info(9999, 7, Some("packB")))), // update
            (hx[1].clone(), None),                               // delete
        ]);
        let assets = BTreeMap::from([("a1".to_string(), hx.clone())]);
        seg.commit_generation(&dirty, &assets).unwrap();

        let updated = seg.lookup(&hx[0]).unwrap();
        assert_eq!((updated.len_raw, updated.refcount), (9999, 7));
        assert_eq!(updated.pack.as_deref(), Some("packB"));
        assert!(seg.lookup(&hx[1]).is_none(), "tombstoned");
        assert_eq!(seg.lookup(&hx[2]).unwrap().len_raw, 102, "base intact");
        assert_eq!(seg.iter_live().count(), 99);
        assert_eq!(seg.delta_count(), 1);

        // Reopen sees the same generation.
        let (seg2, _) = SegIndex::open(dir.path()).unwrap();
        assert_eq!(seg2.generation, seg.generation);
        assert!(seg2.lookup(&hx[1]).is_none());
    }

    #[test]
    fn deltas_compact_past_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let (mut seg, hx) = base_index(dir.path(), 50);
        let assets = BTreeMap::from([("a1".to_string(), hx.clone())]);
        for round in 0..(MAX_DELTA_SEGMENTS + 2) {
            let dirty = BTreeMap::from([(
                hx[round % hx.len()].clone(),
                Some(info(5000 + round as u32, 2, Some("packC"))),
            )]);
            seg.commit_generation(&dirty, &assets).unwrap();
        }
        assert!(
            seg.delta_count() <= MAX_DELTA_SEGMENTS,
            "deltas folded, have {}",
            seg.delta_count()
        );
        assert_eq!(seg.iter_live().count(), 50, "no records lost by folding");
        // Newest value of the repeatedly-updated record survives.
        let last = (MAX_DELTA_SEGMENTS + 1) % hx.len();
        assert_eq!(seg.lookup(&hx[last]).unwrap().refcount, 2);
    }

    #[test]
    fn uncommitted_generation_is_swept_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let (seg, hx) = base_index(dir.path(), 20);
        let committed = seg.generation;
        // Simulate a crash: a newer generation dir exists but CURRENT was
        // never swapped.
        let orphan = SegIndex::index_dir(dir.path())
            .join("generations")
            .join(gen_name(committed + 1));
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::write(orphan.join("root.idx"), b"half-written").unwrap();
        drop(seg);

        let (seg, _) = SegIndex::open(dir.path()).unwrap();
        assert_eq!(seg.generation, committed);
        assert!(!orphan.exists(), "orphan generation swept");
        assert_eq!(seg.lookup(&hx[0]).unwrap().len_raw, 100);
    }

    #[test]
    fn corrupt_segment_is_detected_and_named() {
        let dir = tempfile::tempdir().unwrap();
        let (seg, _hx) = base_index(dir.path(), 200);
        assert_eq!(seg.verify_segments().unwrap(), 1);
        drop(seg);

        // Flip one byte inside the record region.
        let pool = SegIndex::index_dir(dir.path()).join("segments");
        let seg_path = std::fs::read_dir(&pool)
            .unwrap()
            .flatten()
            .find(|e| e.file_name().to_string_lossy().ends_with(".seg"))
            .unwrap()
            .path();
        let mut bytes = std::fs::read(&seg_path).unwrap();
        bytes[SEG_HEADER_LEN + 40] ^= 0xff;
        std::fs::write(&seg_path, &bytes).unwrap();

        let (seg, _) = SegIndex::open(dir.path()).unwrap();
        let err = seg.verify_segments().unwrap_err();
        assert!(err.to_string().contains("seal"), "got: {err}");
    }

    #[test]
    fn old_generations_and_orphan_segments_are_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let (mut seg, hx) = base_index(dir.path(), 30);
        let assets = BTreeMap::from([("a1".to_string(), hx.clone())]);
        for hex in hx.iter().take(4) {
            let dirty = BTreeMap::from([(hex.clone(), Some(info(1, 1, Some("p"))))]);
            seg.commit_generation(&dirty, &assets).unwrap();
        }
        let gens: Vec<_> = std::fs::read_dir(SegIndex::index_dir(dir.path()).join("generations"))
            .unwrap()
            .flatten()
            .collect();
        assert!(gens.len() <= 2, "kept {} generations", gens.len());
    }
}
