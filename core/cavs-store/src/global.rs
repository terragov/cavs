//! Global content-addressable store.
//!
//! One physical copy of each unique chunk across every asset and version,
//! with reference counting and garbage collection. This is what turns the
//! per-`.cavs` egress dedup into real server-side *storage* dedup: ingest
//! v1 and v2 of a build and the bytes they share are stored once.
//!
//! On-disk layout under `root/`:
//! ```text
//!   chunks/<ab>/<hex>        loose layout: one file per chunk, as stored
//!   packs/<ab>/<id>.cavspack packfile layout: chunks appended into large
//!   packs/<ab>/<id>.cavsindex  immutable packs + per-pack sidecar index
//!   assets/<name>.json       per-asset record (tracks/segments by hash),
//!                            for stores on the segmented index and for
//!                            assets published before 1.8
//!   assets/records/<id>.cavsrec  the records of one publish batch, back to
//!                            back, content-addressed and immutable; the
//!                            ledger says which bytes of which file are an
//!                            asset's record. One file per commit rather
//!                            than one per asset.
//!   index.bin                chunk ledger: per chunk {sizes, flags,
//!                            refcount, pack location}; plus the store
//!                            layout. Compact binary snapshot (CAVSIDX1,
//!                            BLAKE3-sealed); pre-1.6 stores used
//!                            index.json, still read and migrated on the
//!                            next save.
//!   index.log                ledger journal: one BLAKE3-sealed record per
//!                            save holding only the entries that save
//!                            touched (CAVSIDL1). A save appends here and
//!                            rewrites index.bin only once the journal has
//!                            outgrown the snapshot it extends, so what a
//!                            save costs is what it changed, not what the
//!                            store holds. Replayed over index.bin on open.
//! ```
//! Chunks are stored in their *stored* (possibly compressed) form so the
//! server can stream them to clients with zero recompression, exactly like
//! the `.cavs` DATA section.
//!
//! The **layout** is fixed at store creation: `loose` (one object per
//! chunk — the pre-0.4.0 behavior, still fully supported) or `packfiles`
//! (chunks appended into content-addressed `.cavspack` files, read by
//! range — see [`crate::packfile`]). A store never mixes semantics: the
//! ledger records where each chunk lives, and reads follow the record.

use crate::packfile::{self, PackWriter, PREFERRED_PACK_SIZE};
use crate::segindex;
use cavs_hash::{from_hex, to_hex, ChunkHash};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("chunk {0} not in store")]
    MissingChunk(String),
    #[error("asset {0} not found")]
    AssetNotFound(String),
    #[error("bad chunk hash {0}")]
    BadHash(String),
    #[error("invalid asset name {0}")]
    BadAssetName(String),
    #[error("corrupt packfile: {0}")]
    PackCorrupt(String),
    #[error("corrupt index: {0}")]
    IndexCorrupt(String),
    #[error("store uses layout {store:?}, requested {requested:?}")]
    LayoutMismatch {
        store: StoreLayout,
        requested: StoreLayout,
    },
    #[error("{0}")]
    NotExportable(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Physical chunk layout, fixed when the store is created.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoreLayout {
    /// One file per chunk under `chunks/<ab>/<hex>` (pre-0.4.0 behavior).
    #[default]
    Loose,
    /// Chunks appended into immutable `.cavspack` files, read by range.
    Packfiles,
}

/// Per-chunk ledger entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkInfo {
    pub len_raw: u32,
    pub len_stored: u32,
    pub flags: u32,
    pub refcount: u64,
    /// Unix epoch seconds when refcount last hit 0 (GC grace anchor).
    #[serde(default)]
    pub zero_since: Option<u64>,
    /// Packfile id (hex) holding this chunk; absent for loose chunks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack: Option<String>,
    /// Offset into the pack's data region, when `pack` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack_offset: Option<u64>,
}

/// Where a chunk physically lives, for manifest location hints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkLocation {
    pub pack_hex: String,
    pub offset: u64,
    pub stored_len: u32,
}

/// Read-efficiency counters of one coalesced batch read.
#[derive(Debug, Clone, Copy, Default)]
pub struct CoalesceStats {
    /// Chunk payloads requested from packfiles.
    pub pack_chunks_requested: u64,
    /// Physical range reads actually issued to packfiles.
    pub pack_ranges_read: u64,
    /// Bytes read from packfiles (≥ bytes served when gaps are included).
    pub pack_bytes_read: u64,
    /// Chunk payload bytes served from packfiles.
    pub pack_bytes_served: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreTrack {
    pub track_id: u32,
    pub kind: u8,
    pub codec: String,
    pub name: String,
    pub timescale: u32,
    pub init_chunks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreSegment {
    pub segment_id: u64,
    pub track_id: u32,
    pub pts_start: u64,
    pub duration: u32,
    pub random_access: bool,
    pub chunks: Vec<String>,
}

/// Everything needed to serve an asset from the store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetRecord {
    pub name: String,
    pub asset_uuid: String,
    pub tracks: Vec<StoreTrack>,
    pub segments: Vec<StoreSegment>,
    pub dict: Vec<String>,
    pub chunk_table: Vec<String>,
    pub merkle_root: String,
    #[serde(default)]
    pub signature: Option<String>,
    #[serde(default)]
    pub signer_pubkey: Option<String>,
    #[serde(default)]
    pub meta: Vec<(String, String)>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Index {
    /// hex -> chunk ledger entry. BTreeMap for stable, diff-friendly json.
    chunks: BTreeMap<String, ChunkInfo>,
    /// asset name -> distinct chunk hexes it references (refcount ledger).
    assets: BTreeMap<String, Vec<String>>,
    /// asset name -> where its record lives, for assets published into a
    /// record pack. An asset with no entry has a flat `assets/<name>.json`.
    #[serde(default)]
    records: BTreeMap<String, RecordRef>,
    /// Physical layout; absent in pre-0.4.0 stores (= loose).
    #[serde(default)]
    layout: StoreLayout,
    /// Monotonic save counter; lets tooling tell which of two snapshots
    /// (`index.bin` vs `index.bin.prev`) is newer without trusting mtimes.
    #[serde(default)]
    generation: u64,
}

/// The bytes of one asset's record inside a record pack
/// (`assets/records/<hex of pack>.cavsrec`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct RecordRef {
    pack: [u8; 32],
    offset: u32,
    len: u32,
}

/// Structure of the ledger, for `store index-inspect`.
#[derive(Debug, Clone, Copy)]
pub struct IndexReport {
    pub segmented: bool,
    pub generation: u64,
    /// Total segments in the active generation (segmented mode only).
    pub segments: usize,
    /// Delta segments awaiting compaction (segmented mode only).
    pub deltas: usize,
    /// Bytes of the ledger journal (`index.log`) the next open will replay
    /// over the snapshot (monolithic mode only).
    pub journal_bytes: u64,
    /// Bytes of the ledger snapshot (`index.bin`) the journal extends
    /// (monolithic mode only).
    pub snapshot_bytes: u64,
}

/// Per-pack fragmentation detail (Round 3D telemetry).
#[derive(Debug, Clone)]
pub struct PackFragmentation {
    pub pack: String,
    pub disk_bytes: u64,
    pub live_bytes: u64,
    pub live_chunks: u64,
    /// `1 - live/disk`: bytes a compaction of this pack would reclaim.
    pub dead_ratio: f64,
}

/// Store-wide fragmentation report: what repacking would buy, before
/// paying for it.
#[derive(Debug, Clone)]
pub struct FragmentationReport {
    pub pack_count: u64,
    /// Packs smaller than [`GlobalStore::SMALL_PACK_BYTES`].
    pub small_packs: u64,
    pub small_pack_ratio: f64,
    pub disk_bytes: u64,
    pub live_bytes: u64,
    pub dead_bytes: u64,
    pub dead_bytes_ratio: f64,
    /// A comparative indicator in [0, 2] (small-pack ratio + dead-bytes
    /// ratio) — meaningful across versions of the same store, not as an
    /// absolute truth.
    pub fragmentation_score: f64,
    pub packs: Vec<PackFragmentation>,
}

/// What a repack pass intends to do (from [`GlobalStore::repack_plan`]).
#[derive(Debug, Clone, Default)]
pub struct RepackPlan {
    /// Groups of small packs to merge into preferred-size packs.
    pub merge_groups: Vec<Vec<String>>,
    /// Packs whose dead-bytes ratio warrants an individual compaction.
    pub compact_packs: Vec<String>,
    pub estimated_read_bytes: u64,
    pub estimated_reclaim_bytes: u64,
}

impl RepackPlan {
    pub fn is_empty(&self) -> bool {
        self.merge_groups.is_empty() && self.compact_packs.is_empty()
    }
}

/// What a repack pass actually did.
#[derive(Debug, Clone, Default)]
pub struct RepackOutcome {
    pub packs_rewritten: u64,
    pub packs_written: u64,
    pub chunks_moved: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub quarantined: Vec<String>,
}

/// Summary for `store stat`.
#[derive(Debug, Clone)]
pub struct StoreStats {
    pub assets: usize,
    pub unique_chunks: u64,
    pub stored_bytes: u64,
    pub unique_raw_bytes: u64,
    /// Bytes that would be stored if every asset kept its own copy.
    pub logical_stored_bytes: u64,
    pub zero_ref_chunks: u64,
    pub layout: StoreLayout,
    /// Packfile layout only: pack files on disk and their total size.
    pub pack_count: u64,
    pub pack_disk_bytes: u64,
    /// Stored bytes of live (referenced) chunks inside packs; the gap to
    /// `pack_disk_bytes` is dead weight reclaimable when a pack fully dies.
    pub pack_live_bytes: u64,
}

/// The store's chunk table when it runs on the segmented index (Round 3B):
/// the mmapped generations plus an in-RAM overlay of records touched since
/// the last commit (`None` = deletion). Reads consult the overlay first;
/// [`GlobalStore::save_index`] turns the overlay into one delta segment.
struct SegState {
    index: segindex::SegIndex,
    overlay: BTreeMap<String, Option<ChunkInfo>>,
}

pub struct GlobalStore {
    root: PathBuf,
    index: Index,
    /// `Some` = segmented-index mode: `index.chunks` stays empty and chunk
    /// lookups go through the mmapped segments + overlay instead of RAM.
    seg: Option<SegState>,
    open_pack: Option<PackWriter>,
    preferred_pack_size: u64,
    /// `Some` while a publish batch is open (see
    /// [`Self::begin_publish_batch`]): asset records queued for the commit.
    batch: Option<Vec<AssetRecord>>,
    /// Ledger entries touched since the last save (monolithic mode). A save
    /// appends exactly these to the journal instead of rewriting the
    /// snapshot; see [`Self::save_index`].
    dirty_chunks: BTreeSet<String>,
    dirty_assets: BTreeSet<String>,
    /// Size of `index.log` as of the last save or open, and of the snapshot
    /// it extends. A save compares the two to decide when the journal has
    /// grown past what it is saving and a fresh snapshot is cheaper to
    /// replay. `snapshot_bytes == 0` means there is no snapshot for a
    /// journal to extend (a ledger loaded from a pre-1.6 `index.json` or a
    /// v1 snapshot, or recovered from the previous generation), and the next
    /// save writes one.
    journal_bytes: u64,
    snapshot_bytes: u64,
    /// Overrides the journal budget; see [`Self::set_journal_budget`].
    journal_budget: Option<u64>,
}

impl GlobalStore {
    /// Open (or create) a store rooted at `root`, keeping its layout.
    pub fn open(root: &Path) -> Result<Self> {
        Self::open_with_layout(root, None)
    }

    /// Open a store; `layout` is applied only when the store is newly
    /// created. Opening an existing store with a *different* requested
    /// layout is an error (a store never changes layout in place).
    pub fn open_with_layout(root: &Path, layout: Option<StoreLayout>) -> Result<Self> {
        std::fs::create_dir_all(root.join("chunks"))?;
        std::fs::create_dir_all(root.join("assets"))?;

        // Segmented-index stores (Round 3B, opted in via
        // [`Self::migrate_index_to_segmented`]) open by mmap: the chunk
        // table never loads into RAM.
        if segindex::SegIndex::exists(root) {
            let (seg, assets) = segindex::SegIndex::open(root)?;
            if let Some(requested) = layout {
                if requested != seg.layout {
                    return Err(StoreError::LayoutMismatch {
                        store: seg.layout,
                        requested,
                    });
                }
            }
            let index = Index {
                chunks: BTreeMap::new(), // unused in segmented mode
                assets,
                records: BTreeMap::new(), // the segmented index keeps flat records
                layout: seg.layout,
                generation: seg.generation,
            };
            Self::sweep_part_packs(root)?;
            let store = Self {
                root: root.to_path_buf(),
                index,
                seg: Some(SegState {
                    index: seg,
                    overlay: BTreeMap::new(),
                }),
                open_pack: None,
                preferred_pack_size: PREFERRED_PACK_SIZE,
                batch: None,
                dirty_chunks: BTreeSet::new(),
                dirty_assets: BTreeSet::new(),
                journal_bytes: 0,
                snapshot_bytes: 0,
                journal_budget: None,
            };
            store.restore_quarantined_packs()?;
            return Ok(store);
        }

        let bin_path = root.join("index.bin");
        let prev_path = root.join("index.bin.prev");
        let json_path = root.join("index.json");
        // A crash mid-save can leave a temp snapshot behind; the live ledger
        // was never touched, so it is safe to drop.
        let _ = std::fs::remove_file(bin_path.with_extension("bin.tmp"));
        let log_path = root.join(JOURNAL_FILE);
        let log_prev_path = root.join(JOURNAL_PREV_FILE);
        let (mut index, snapshot_bytes, mut journal_bytes) =
            match Self::load_ledger(&bin_path, &prev_path, &json_path)? {
                Some((index, LedgerSource::Live(bytes))) => (index, bytes, 0),
                Some((mut index, LedgerSource::Prev)) => {
                    // The live snapshot is gone: the journal it superseded
                    // (rotated to `.prev` when it was written) carries the
                    // saves between the two snapshots, and the live journal
                    // whatever came after the lost one — which cannot apply
                    // without it and stops at the gap. The store reads as it
                    // did one snapshot ago at worst, and the next save writes
                    // a fresh snapshot rather than extending a recovered one.
                    replay_journal(&mut index, &log_prev_path)?;
                    replay_journal(&mut index, &log_path)?;
                    (index, 0, 0)
                }
                Some((index, LedgerSource::Legacy)) | Some((index, LedgerSource::Json)) => {
                    // A pre-1.8 ledger never had a journal; anything by that
                    // name is a stray and must not be replayed over it.
                    let _ = std::fs::remove_file(&log_path);
                    let _ = std::fs::remove_file(&log_prev_path);
                    (index, 0, 0)
                }
                None => {
                    let index = Index {
                        layout: layout.unwrap_or_default(),
                        ..Index::default()
                    };
                    // A journal without a snapshot describes nothing this
                    // store can extend.
                    let _ = std::fs::remove_file(&log_path);
                    let _ = std::fs::remove_file(&log_prev_path);
                    // Persist immediately: the layout is a creation-time property
                    // and must survive even if nothing is published yet.
                    let tmp = bin_path.with_extension("bin.tmp");
                    let encoded = encode_index(&index);
                    std::fs::write(&tmp, &encoded)?;
                    std::fs::rename(&tmp, &bin_path)?;
                    (index, encoded.len() as u64, 0)
                }
            };
        if snapshot_bytes > 0 {
            // The live snapshot loaded: replay the saves made since it was
            // written. A torn or corrupt tail — a crash mid-append — is cut
            // off so the next append starts from a record boundary rather
            // than behind bytes no replay will ever get past.
            let scan = replay_journal(&mut index, &log_path)?;
            if scan.truncate_to < scan.file_len {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&log_path)?
                    .set_len(scan.truncate_to)?;
            }
            journal_bytes = scan.truncate_to;
        }
        if let Some(requested) = layout {
            if requested != index.layout && (bin_path.exists() || json_path.exists()) {
                return Err(StoreError::LayoutMismatch {
                    store: index.layout,
                    requested,
                });
            }
        }
        Self::sweep_part_packs(root)?;
        let store = Self {
            root: root.to_path_buf(),
            index,
            seg: None,
            open_pack: None,
            preferred_pack_size: PREFERRED_PACK_SIZE,
            batch: None,
            dirty_chunks: BTreeSet::new(),
            dirty_assets: BTreeSet::new(),
            journal_bytes,
            snapshot_bytes,
            journal_budget: None,
        };
        // A ledger recovered from a previous generation may reference packs
        // a newer GC had already quarantined; bring them back.
        store.restore_quarantined_packs()?;
        Ok(store)
    }

    /// A crash mid-ingest can leave a temp pack behind; it was never
    /// referenced by the ledger, so it is safe to drop.
    fn sweep_part_packs(root: &Path) -> Result<()> {
        let packs_dir = root.join("packs");
        if packs_dir.is_dir() {
            for entry in std::fs::read_dir(&packs_dir)?.flatten() {
                if entry.path().extension().is_some_and(|e| e == "part") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        // Likewise a record pack a crash left half-staged.
        let records_dir = record_packs_dir(root);
        if records_dir.is_dir() {
            for entry in std::fs::read_dir(&records_dir)?.flatten() {
                if entry.path().extension().is_some_and(|e| e == "tmp") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        Ok(())
    }

    /// Migrate this store's ledger from the monolithic `index.bin` to the
    /// segmented, mmapped index (Round 3B). One-way and explicit: the old
    /// snapshot is kept as `index.bin.pre-migration` for a manual rollback
    /// (delete `index/` and rename it back). Subsequent opens go straight
    /// to the segmented path; publishes append delta segments instead of
    /// rewriting the ledger. Returns the migrated chunk count.
    pub fn migrate_index_to_segmented(&mut self) -> Result<u64> {
        if self.seg.is_some() {
            return Ok(self.chunks_len()); // already segmented
        }
        // Resolve every pending location so the migrated records are final.
        self.flush_packs()?;
        // The segmented index keeps one record file per asset; give every
        // asset that lives in a record pack its own file before the ledger
        // that knew where it was is replaced.
        let packed: Vec<String> = self.index.records.keys().cloned().collect();
        for name in &packed {
            let bytes = self.asset_record_bytes(name)?;
            let path = self.root.join("assets").join(format!("{name}.json"));
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, &path)?;
        }
        self.index.records.clear();
        let migrated = self.index.chunks.len() as u64;
        // One self-contained legacy snapshot for rollback. Written fresh
        // rather than renamed from `index.bin`, which may be a journal
        // behind the ledger being migrated.
        let rollback = encode_index(&self.index);
        let (seg, _assets) = segindex::SegIndex::create(
            &self.root,
            self.index.generation + 1,
            self.index.layout,
            &self.index.chunks,
            &self.index.assets,
        )?;
        self.index.generation = seg.generation;
        self.seg = Some(SegState {
            index: seg,
            overlay: BTreeMap::new(),
        });
        self.index.chunks = BTreeMap::new();
        self.dirty_chunks.clear();
        self.dirty_assets.clear();
        self.journal_bytes = 0;
        self.snapshot_bytes = 0;
        // Keep the rollback snapshot; remove the rest so a pre-3B binary
        // cannot silently open a stale ledger.
        std::fs::write(self.root.join("index.bin.pre-migration"), rollback)?;
        let _ = std::fs::remove_file(self.root.join("index.bin"));
        let _ = std::fs::remove_file(self.root.join("index.bin.prev"));
        let _ = std::fs::remove_file(self.root.join("index.json"));
        let _ = std::fs::remove_file(self.root.join(JOURNAL_FILE));
        let _ = std::fs::remove_file(self.root.join(JOURNAL_PREV_FILE));
        let _ = std::fs::remove_dir_all(record_packs_dir(&self.root));
        Ok(migrated)
    }

    /// Whether this store runs on the segmented (mmap) index.
    pub fn is_segmented(&self) -> bool {
        self.seg.is_some()
    }

    /// Packs below this size are merge candidates: many small packs mean
    /// many pack switches, ranges and HTTP requests per reconstruction.
    pub const SMALL_PACK_BYTES: u64 = 8 * 1024 * 1024;
    /// A pack whose dead-bytes ratio exceeds this is a compaction candidate.
    pub const DEAD_RATIO_THRESHOLD: f64 = 0.30;

    /// Round 3D fragmentation telemetry: one streaming ledger pass + one
    /// `stat` per referenced pack. Makes repacking a measured decision
    /// instead of a guess.
    pub fn fragmentation(&self) -> FragmentationReport {
        let mut live: HashMap<String, (u64, u64)> = HashMap::new(); // pack -> (bytes, chunks)
        for (_, info) in self.chunks_iter() {
            if let Some(pack) = info.pack {
                let e = live.entry(pack).or_default();
                e.0 += info.len_stored as u64;
                e.1 += 1;
            }
        }
        let mut packs: Vec<PackFragmentation> = live
            .into_iter()
            .map(|(pack, (live_bytes, live_chunks))| {
                let disk_bytes = std::fs::metadata(packfile::pack_path(&self.packs_dir(), &pack))
                    .map(|m| m.len())
                    .unwrap_or(0);
                let dead_ratio = if disk_bytes == 0 {
                    0.0
                } else {
                    // Header/footer overhead is not "dead"; clamp at 0.
                    1.0 - (live_bytes.min(disk_bytes) as f64 / disk_bytes as f64)
                };
                PackFragmentation {
                    pack,
                    disk_bytes,
                    live_bytes,
                    live_chunks,
                    dead_ratio,
                }
            })
            .collect();
        packs.sort_by(|a, b| b.dead_ratio.total_cmp(&a.dead_ratio));

        let pack_count = packs.len() as u64;
        let small_packs = packs
            .iter()
            .filter(|p| p.disk_bytes < Self::SMALL_PACK_BYTES)
            .count() as u64;
        let disk_bytes: u64 = packs.iter().map(|p| p.disk_bytes).sum();
        let live_bytes: u64 = packs.iter().map(|p| p.live_bytes).sum();
        let dead_bytes = disk_bytes.saturating_sub(live_bytes);
        let small_pack_ratio = if pack_count == 0 {
            0.0
        } else {
            small_packs as f64 / pack_count as f64
        };
        let dead_bytes_ratio = if disk_bytes == 0 {
            0.0
        } else {
            dead_bytes as f64 / disk_bytes as f64
        };
        FragmentationReport {
            pack_count,
            small_packs,
            small_pack_ratio,
            disk_bytes,
            live_bytes,
            dead_bytes,
            dead_bytes_ratio,
            fragmentation_score: small_pack_ratio + dead_bytes_ratio,
            packs,
        }
    }

    /// First-generation repack planner: merge small packs up to the
    /// preferred pack size, and compact packs past the dead-bytes
    /// threshold. Pack affinity by access telemetry is deliberately out of
    /// scope (Round 4).
    pub fn repack_plan(&self) -> RepackPlan {
        let frag = self.fragmentation();
        let mut plan = RepackPlan::default();
        let mut merge_batch: Vec<String> = Vec::new();
        let mut batch_bytes = 0u64;
        for p in &frag.packs {
            if p.disk_bytes < Self::SMALL_PACK_BYTES {
                if batch_bytes + p.live_bytes > self.preferred_pack_size && merge_batch.len() > 1 {
                    plan.merge_groups.push(std::mem::take(&mut merge_batch));
                    batch_bytes = 0;
                }
                merge_batch.push(p.pack.clone());
                batch_bytes += p.live_bytes;
                plan.estimated_read_bytes += p.live_bytes;
                plan.estimated_reclaim_bytes += p.disk_bytes.saturating_sub(p.live_bytes);
            } else if p.dead_ratio > Self::DEAD_RATIO_THRESHOLD {
                plan.compact_packs.push(p.pack.clone());
                plan.estimated_read_bytes += p.live_bytes;
                plan.estimated_reclaim_bytes += p.disk_bytes.saturating_sub(p.live_bytes);
            }
        }
        // A merge needs at least two packs to be worth a rewrite.
        if merge_batch.len() > 1 {
            plan.merge_groups.push(merge_batch);
        }
        plan
    }

    /// Execute a repack plan, copy-on-write: live chunks of each group are
    /// rewritten into fresh packs, the ledger swaps to a new generation,
    /// and only then are the old packs quarantined (recoverable for the
    /// whole quarantine window — a crash at any point loses nothing).
    /// Reads keep working throughout: old packs stay in place until the
    /// ledger no longer references them.
    ///
    /// Note: exported static trees hold copies of the old packs; re-export
    /// affected assets (and their meta-packs) after a repack.
    pub fn repack_run(&mut self, plan: &RepackPlan, dry_run: bool) -> Result<RepackOutcome> {
        let mut outcome = RepackOutcome::default();
        if plan.is_empty() {
            return Ok(outcome);
        }
        // Never interleave with an open ingest pack.
        self.flush_packs()?;

        let mut groups: Vec<Vec<String>> = plan.merge_groups.clone();
        groups.extend(plan.compact_packs.iter().map(|p| vec![p.clone()]));

        for group in groups {
            let members: HashSet<&str> = group.iter().map(String::as_str).collect();
            // Live chunks of this group, in physical order (locality kept).
            let mut chunks: Vec<(String, ChunkInfo)> = self
                .chunks_iter()
                .filter(|(_, i)| i.pack.as_deref().is_some_and(|p| members.contains(p)))
                .collect();
            chunks.sort_by(|a, b| {
                (a.1.pack.as_deref(), a.1.pack_offset).cmp(&(b.1.pack.as_deref(), b.1.pack_offset))
            });
            outcome.packs_rewritten += group.len() as u64;
            if dry_run {
                outcome.chunks_moved += chunks.len() as u64;
                outcome.bytes_read += chunks.iter().map(|(_, i)| i.len_stored as u64).sum::<u64>();
                continue;
            }

            // Copy live chunks into fresh packs (rolling over at the
            // preferred size), then repoint the ledger.
            let mut writer: Option<PackWriter> = None;
            let mut finished: Vec<(String, Vec<packfile::PackEntry>)> = Vec::new();
            for (hex, _) in &chunks {
                let hash = from_hex(hex).ok_or_else(|| StoreError::BadHash(hex.clone()))?;
                let (stored, flags, len_raw) = self.read_chunk_stored(&hash)?;
                outcome.bytes_read += stored.len() as u64;
                if writer.is_none() {
                    writer = Some(PackWriter::create(&self.packs_dir())?);
                }
                let w = writer.as_mut().unwrap();
                w.append(hash, &stored, len_raw, flags)?;
                outcome.bytes_written += stored.len() as u64;
                if w.data_len() >= self.preferred_pack_size {
                    let (pack_hex, entries) = writer.take().unwrap().finish()?;
                    finished.push((pack_hex, entries));
                }
            }
            if let Some(w) = writer.take() {
                if w.is_empty() {
                    w.abort();
                } else {
                    let (pack_hex, entries) = w.finish()?;
                    finished.push((pack_hex, entries));
                }
            }
            outcome.chunks_moved += chunks.len() as u64;
            outcome.packs_written += finished.len() as u64;

            // Repoint every moved chunk at its new pack, then persist the
            // ledger before touching the old packs.
            for (pack_hex, entries) in &finished {
                for entry in entries {
                    let hex = to_hex(&entry.hash);
                    self.chunk_update(&hex, |info| {
                        info.pack = Some(pack_hex.clone());
                        info.pack_offset = Some(entry.offset);
                    });
                }
            }
            self.save_index()?;
            for pack in &group {
                self.quarantine_pack(pack)?;
                outcome.quarantined.push(pack.clone());
            }
        }
        Ok(outcome)
    }

    /// A small structural report of the ledger, for `store index-inspect`.
    pub fn index_report(&self) -> IndexReport {
        match &self.seg {
            Some(seg) => IndexReport {
                segmented: true,
                generation: seg.index.generation,
                segments: seg.index.segment_count(),
                deltas: seg.index.delta_count(),
                journal_bytes: 0,
                snapshot_bytes: 0,
            },
            None => IndexReport {
                segmented: false,
                generation: self.index.generation,
                segments: 0,
                deltas: 0,
                journal_bytes: self.journal_bytes,
                snapshot_bytes: self.snapshot_bytes,
            },
        }
    }

    /// Load the ledger, preferring `index.bin` and falling back to the
    /// previous generation (`index.bin.prev`) if the current snapshot is
    /// corrupt or missing (a crash between the two renames of
    /// [`Self::save_index`] leaves only `.prev`). A legacy `index.json`
    /// (pre-1.6) is read as a last resort and migrated on the next save.
    /// Returns `Ok(None)` when no ledger exists at all (a new store), and
    /// otherwise which of the three it came from, since only the live
    /// snapshot is something the journal can extend.
    fn load_ledger(bin: &Path, prev: &Path, json: &Path) -> Result<Option<(Index, LedgerSource)>> {
        let current = if bin.exists() {
            let bytes = std::fs::read(bin)?;
            match decode_index_versioned(&bytes) {
                // A snapshot older than the journal cannot have one, and must
                // not be given one: a reader of its version would open it and
                // miss every save the journal held. Its first save rewrites it.
                Ok((index, version)) if version < 2 => {
                    return Ok(Some((index, LedgerSource::Legacy)))
                }
                Ok((index, _)) => return Ok(Some((index, LedgerSource::Live(bytes.len() as u64)))),
                Err(e) => Some(e), // corrupt: try the previous generation
            }
        } else {
            None
        };
        if prev.exists() {
            match decode_index(&std::fs::read(prev)?) {
                Ok(index) => return Ok(Some((index, LedgerSource::Prev))),
                Err(prev_err) => {
                    // Both generations bad: surface the current one's error
                    // (or the prev error when index.bin never existed).
                    return Err(current.unwrap_or(prev_err));
                }
            }
        }
        if let Some(e) = current {
            return Err(e);
        }
        if json.exists() {
            let index = serde_json::from_slice::<Index>(&std::fs::read(json)?)?;
            return Ok(Some((index, LedgerSource::Json)));
        }
        Ok(None)
    }

    /// Begin a publish batch (session-scoped, Xet-style finalize): until
    /// [`Self::commit_publish_batch`], `publish_asset` only updates the
    /// in-memory ledger — the ingest pack stays open across assets (so many
    /// small assets aggregate into few large packs instead of one pack per
    /// asset), asset record files are not written, and `index.json` is not
    /// saved. If the process dies before the commit, the on-disk store is
    /// exactly as it was before the batch (orphan `.part` packs are swept on
    /// the next open), so an interrupted push simply re-ingests.
    pub fn begin_publish_batch(&mut self) {
        if self.batch.is_none() {
            self.batch = Some(Vec::new());
        }
    }

    /// Persist everything the open publish batch deferred: close the ingest
    /// pack (resolving ledger locations), write every queued asset record,
    /// and save the ledger once — one `index.json` write per push session
    /// instead of one per object. Idempotent; a no-op when no batch is open.
    pub fn commit_publish_batch(&mut self) -> Result<()> {
        let Some(pending) = self.batch.take() else {
            return Ok(());
        };
        let had_open_pack = self.open_pack.is_some();
        self.flush_packs()?;
        self.write_asset_records(&pending)?;
        if !pending.is_empty() || had_open_pack {
            self.save_index()?;
        }
        Ok(())
    }

    /// Whether an asset is published — including assets queued in an open
    /// publish batch (unlike [`Self::get_asset`], which reads the record
    /// file a batch has not written yet).
    pub fn has_asset(&self, name: &str) -> bool {
        self.index.assets.contains_key(name)
    }

    pub fn layout(&self) -> StoreLayout {
        self.index.layout
    }

    /// Override the pack rollover size (tests use small packs).
    pub fn set_preferred_pack_size(&mut self, bytes: u64) {
        self.preferred_pack_size = bytes.max(1);
    }

    /// Override how large the ledger journal may grow before a save writes
    /// a fresh snapshot instead of appending. The default is the size of the
    /// snapshot itself, and never below [`JOURNAL_MIN_BYTES`]; `0` makes
    /// every save a snapshot, which is what the pre-1.8 store did. Tests use
    /// it to exercise the rollover; a store that is only ever opened to be
    /// exported may prefer it too.
    pub fn set_journal_budget(&mut self, bytes: u64) {
        self.journal_budget = Some(bytes);
    }

    fn journal_budget(&self) -> u64 {
        self.journal_budget
            .unwrap_or_else(|| self.snapshot_bytes.max(JOURNAL_MIN_BYTES))
    }

    fn chunk_path(&self, hex: &str) -> PathBuf {
        self.root.join("chunks").join(&hex[..2]).join(hex)
    }

    fn packs_dir(&self) -> PathBuf {
        self.root.join("packs")
    }

    // ------------------------------------------------------------------
    // Chunk-table accessors: the single seam between the store's logic and
    // its ledger representation (in-RAM BTreeMap, or mmapped segments +
    // overlay). Everything below this block goes through these.
    // ------------------------------------------------------------------

    fn chunk_get(&self, hex: &str) -> Option<ChunkInfo> {
        match &self.seg {
            Some(seg) => match seg.overlay.get(hex) {
                Some(entry) => entry.clone(), // Some(None) = deleted
                None => seg.index.lookup(hex),
            },
            None => self.index.chunks.get(hex).cloned(),
        }
    }

    fn chunk_contains(&self, hex: &str) -> bool {
        match &self.seg {
            Some(seg) => match seg.overlay.get(hex) {
                Some(entry) => entry.is_some(),
                None => seg.index.lookup(hex).is_some(),
            },
            None => self.index.chunks.contains_key(hex),
        }
    }

    fn chunk_insert(&mut self, hex: String, info: ChunkInfo) {
        match &mut self.seg {
            Some(seg) => {
                seg.overlay.insert(hex, Some(info));
            }
            None => {
                self.dirty_chunks.insert(hex.clone());
                self.index.chunks.insert(hex, info);
            }
        }
    }

    fn chunk_remove(&mut self, hex: &str) -> Option<ChunkInfo> {
        match &mut self.seg {
            Some(_) => {
                let old = self.chunk_get(hex)?;
                self.seg
                    .as_mut()
                    .unwrap()
                    .overlay
                    .insert(hex.to_string(), None); // tombstone
                Some(old)
            }
            None => {
                let old = self.index.chunks.remove(hex);
                if old.is_some() {
                    self.dirty_chunks.insert(hex.to_string());
                }
                old
            }
        }
    }

    /// Read-modify-write one entry; returns whether it existed.
    fn chunk_update<F: FnOnce(&mut ChunkInfo)>(&mut self, hex: &str, f: F) -> bool {
        match &mut self.seg {
            Some(_) => {
                let Some(mut info) = self.chunk_get(hex) else {
                    return false;
                };
                f(&mut info);
                self.seg
                    .as_mut()
                    .unwrap()
                    .overlay
                    .insert(hex.to_string(), Some(info));
                true
            }
            None => match self.index.chunks.get_mut(hex) {
                Some(info) => {
                    f(info);
                    self.dirty_chunks.insert(hex.to_string());
                    true
                }
                None => false,
            },
        }
    }

    /// Every live ledger entry, sorted by hex. In segmented mode this
    /// streams a k-way merge over the mmaps shadowed by the overlay —
    /// nothing is materialized.
    fn chunks_iter(&self) -> Box<dyn Iterator<Item = (String, ChunkInfo)> + '_> {
        match &self.seg {
            Some(seg) => Box::new(OverlayMerge {
                base: seg.index.iter_live().peekable(),
                overlay: seg.overlay.iter().peekable(),
            }),
            None => Box::new(
                self.index
                    .chunks
                    .iter()
                    .map(|(hex, info)| (hex.clone(), info.clone())),
            ),
        }
    }

    fn chunks_len(&self) -> u64 {
        match &self.seg {
            Some(_) => self.chunks_iter().count() as u64,
            None => self.index.chunks.len() as u64,
        }
    }

    /// The set of pack ids any live chunk references (GC / quarantine /
    /// export all reason about liveness at pack granularity).
    fn live_pack_set(&self) -> HashSet<String> {
        self.chunks_iter()
            .filter_map(|(_, info)| info.pack)
            .collect()
    }

    pub fn has_chunk(&self, hash: &ChunkHash) -> bool {
        self.chunk_contains(&to_hex(hash))
    }

    pub fn chunk_info(&self, hash: &ChunkHash) -> Option<ChunkInfo> {
        self.chunk_get(&to_hex(hash))
    }

    /// Store a chunk in its stored form. No-op (returns false) if already
    /// present. New chunks enter with refcount 0 until an asset is published.
    ///
    /// In the packfile layout the chunk is appended to the currently open
    /// pack; its ledger location is resolved when the pack closes (on
    /// rollover, or at the latest inside [`Self::publish_asset`]).
    pub fn put_chunk(
        &mut self,
        hash: &ChunkHash,
        stored: &[u8],
        flags: u32,
        len_raw: u32,
    ) -> Result<bool> {
        let hex = to_hex(hash);
        if self.chunk_contains(&hex) {
            return Ok(false);
        }
        let entry = ChunkInfo {
            len_raw,
            len_stored: stored.len() as u32,
            flags,
            refcount: 0,
            zero_since: Some(0),
            pack: None,
            pack_offset: None,
        };
        match self.index.layout {
            StoreLayout::Loose => {
                let path = self.chunk_path(&hex);
                std::fs::create_dir_all(path.parent().unwrap())?;
                let tmp = path.with_extension("tmp");
                std::fs::write(&tmp, stored)?;
                std::fs::rename(&tmp, &path)?;
                self.chunk_insert(hex, entry);
            }
            StoreLayout::Packfiles => {
                if self.open_pack.is_none() {
                    self.open_pack = Some(PackWriter::create(&self.packs_dir())?);
                }
                let writer = self.open_pack.as_mut().unwrap();
                writer.append(*hash, stored, len_raw, flags)?;
                let full = writer.data_len() >= self.preferred_pack_size;
                // Ledger entry first (location unresolved), so the flush
                // below — and any later one — fills in pack/offset.
                self.chunk_insert(hex, entry);
                if full {
                    self.flush_packs()?;
                }
            }
        }
        Ok(true)
    }

    /// Close the currently open pack, if any, resolving the ledger
    /// locations of every chunk it holds. Idempotent.
    pub fn flush_packs(&mut self) -> Result<()> {
        let Some(writer) = self.open_pack.take() else {
            return Ok(());
        };
        if writer.is_empty() {
            writer.abort();
            return Ok(());
        }
        let (pack_hex, entries) = writer.finish()?;
        for entry in entries {
            let hex = to_hex(&entry.hash);
            let resolved = self.chunk_update(&hex, |info| {
                info.pack = Some(pack_hex.clone());
                info.pack_offset = Some(entry.offset);
            });
            // put_chunk always inserts the entry before flushing, so this
            // arm is defensive (e.g. a future caller flushing a writer it
            // fed directly).
            if !resolved {
                self.chunk_insert(
                    hex,
                    ChunkInfo {
                        len_raw: entry.raw_len,
                        len_stored: entry.stored_len,
                        flags: entry.flags,
                        refcount: 0,
                        zero_since: Some(0),
                        pack: Some(pack_hex.clone()),
                        pack_offset: Some(entry.offset),
                    },
                );
            }
        }
        Ok(())
    }

    /// Read a chunk in its stored form: (stored bytes, flags, len_raw).
    pub fn read_chunk_stored(&self, hash: &ChunkHash) -> Result<(Vec<u8>, u32, u32)> {
        let hex = to_hex(hash);
        let info = self
            .chunk_get(&hex)
            .ok_or_else(|| StoreError::MissingChunk(hex.clone()))?;
        let bytes = match (&info.pack, info.pack_offset) {
            (Some(pack), Some(offset)) => packfile::read_pack_range(
                &packfile::pack_path(&self.packs_dir(), pack),
                offset,
                info.len_stored as u64,
            )?,
            _ => std::fs::read(self.chunk_path(&hex))
                .map_err(|_| StoreError::MissingChunk(hex.clone()))?,
        };
        Ok((bytes, info.flags, info.len_raw))
    }

    /// Where a chunk physically lives, when it lives in a pack (manifest
    /// location hints).
    pub fn chunk_location(&self, hash: &ChunkHash) -> Option<ChunkLocation> {
        let info = self.chunk_get(&to_hex(hash))?;
        Some(ChunkLocation {
            pack_hex: info.pack.clone()?,
            offset: info.pack_offset?,
            stored_len: info.len_stored,
        })
    }

    /// Maximum dead space between two chunks that still coalesces into one
    /// physical read.
    const MAX_COALESCE_GAP: u64 = 64 * 1024;
    /// Upper bound of one coalesced read.
    const MAX_COALESCED_RANGE: u64 = 8 * 1024 * 1024;

    /// Read many chunks (stored form), coalescing pack reads: chunks from
    /// the same pack whose ranges are within [`Self::MAX_COALESCE_GAP`] of
    /// each other are fetched with a single physical read (capped at
    /// [`Self::MAX_COALESCED_RANGE`]). Results keep the input order; loose
    /// chunks read individually. Returns per-batch efficiency counters.
    #[allow(clippy::type_complexity)]
    pub fn read_chunks_stored_batch(
        &self,
        hashes: &[ChunkHash],
    ) -> Result<(Vec<(Vec<u8>, u32, u32)>, CoalesceStats)> {
        let mut out: Vec<Option<(Vec<u8>, u32, u32)>> = vec![None; hashes.len()];
        let mut stats = CoalesceStats::default();
        // pack hex -> (input position, offset, stored_len, flags, len_raw)
        let mut by_pack: HashMap<String, Vec<(usize, u64, u32, u32, u32)>> = HashMap::new();

        for (pos, hash) in hashes.iter().enumerate() {
            let hex = to_hex(hash);
            let info = self
                .chunk_get(&hex)
                .ok_or_else(|| StoreError::MissingChunk(hex.clone()))?;
            match (&info.pack, info.pack_offset) {
                (Some(pack), Some(offset)) => {
                    by_pack.entry(pack.clone()).or_default().push((
                        pos,
                        offset,
                        info.len_stored,
                        info.flags,
                        info.len_raw,
                    ));
                }
                _ => {
                    let bytes = std::fs::read(self.chunk_path(&hex))
                        .map_err(|_| StoreError::MissingChunk(hex.clone()))?;
                    out[pos] = Some((bytes, info.flags, info.len_raw));
                }
            }
        }

        for (pack, mut chunks) in by_pack {
            let pack_file = packfile::pack_path(&self.packs_dir(), &pack);
            chunks.sort_by_key(|&(_, offset, ..)| offset);
            stats.pack_chunks_requested += chunks.len() as u64;

            let mut i = 0;
            while i < chunks.len() {
                // Grow the range while the next chunk is close enough and
                // the merged read stays under the cap.
                let start = chunks[i].1;
                let mut end = chunks[i].1 + chunks[i].2 as u64;
                let mut j = i + 1;
                while j < chunks.len() {
                    let (_, offset, stored_len, ..) = chunks[j];
                    let chunk_end = offset + stored_len as u64;
                    if offset.saturating_sub(end) > Self::MAX_COALESCE_GAP
                        || chunk_end.max(end) - start > Self::MAX_COALESCED_RANGE
                    {
                        break;
                    }
                    end = end.max(chunk_end);
                    j += 1;
                }
                let range = packfile::read_pack_range(&pack_file, start, end - start)?;
                stats.pack_ranges_read += 1;
                stats.pack_bytes_read += end - start;
                for &(pos, offset, stored_len, flags, len_raw) in &chunks[i..j] {
                    let lo = (offset - start) as usize;
                    let bytes = range[lo..lo + stored_len as usize].to_vec();
                    stats.pack_bytes_served += stored_len as u64;
                    out[pos] = Some((bytes, flags, len_raw));
                }
                i = j;
            }
        }

        Ok((out.into_iter().map(|c| c.unwrap()).collect(), stats))
    }

    /// Publish (or replace) an asset. Refcounts are adjusted so the chunk
    /// ledger reflects exactly the currently-published assets.
    ///
    /// Inside a publish batch (see [`Self::begin_publish_batch`]) only the
    /// in-memory ledger changes; the ingest pack stays open and nothing is
    /// persisted until [`Self::commit_publish_batch`].
    pub fn publish_asset(&mut self, record: &AssetRecord) -> Result<()> {
        if record.name.contains(['/', '\\', '.']) || record.name.is_empty() {
            return Err(StoreError::BadAssetName(record.name.clone()));
        }
        let batching = self.batch.is_some();
        if !batching {
            // Close the ingest pack so every chunk has a resolved location
            // before the ledger is persisted.
            self.flush_packs()?;
        }
        // Distinct chunks this asset references.
        let mut distinct: HashSet<String> = HashSet::new();
        for t in &record.tracks {
            distinct.extend(t.init_chunks.iter().cloned());
        }
        for s in &record.segments {
            distinct.extend(s.chunks.iter().cloned());
        }
        // Validate every referenced chunk exists.
        for hex in &distinct {
            if !self.chunk_contains(hex) {
                return Err(StoreError::MissingChunk(hex.clone()));
            }
        }
        // Replacing: drop old refs first.
        if let Some(old) = self.index.assets.remove(&record.name) {
            self.decrement(&old);
        }
        self.dirty_assets.insert(record.name.clone());
        for hex in &distinct {
            self.chunk_update(hex, |info| {
                info.refcount += 1;
                info.zero_since = None;
            });
        }
        self.index
            .assets
            .insert(record.name.clone(), distinct.into_iter().collect());
        if batching {
            self.batch.as_mut().unwrap().push(record.clone());
            return Ok(());
        }
        self.write_asset_records(std::slice::from_ref(record))?;
        self.save_index()
    }

    /// Persist the records of a publish. On the monolithic ledger they go
    /// into one content-addressed record pack and the ledger remembers the
    /// bytes of each — one file and one fsync per publish where there used
    /// to be four filesystem calls per asset, in a directory holding every
    /// asset the store has, which a batch of a few hundred spent longer on
    /// than on the ledger it was also writing. The segmented index keeps the
    /// flat `assets/<name>.json` files; its own commit does not carry a
    /// record table.
    fn write_asset_records(&mut self, records: &[AssetRecord]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        if self.seg.is_some() {
            return write_flat_asset_records(&self.root, records);
        }
        // Every record has an asset, so a store with more assets than
        // records still has flat files from before 1.8, and a republish
        // must take the flat copy with it — a reader that lost the ledger
        // would otherwise find the stale one. A store past that point skips
        // the unlink per asset.
        let flat_records_remain = self.index.assets.len() > self.index.records.len();
        for (name, at) in write_record_pack(&self.root, records)? {
            if flat_records_remain {
                let flat = self.root.join("assets").join(format!("{name}.json"));
                let _ = std::fs::remove_file(flat);
            }
            self.index.records.insert(name, at);
        }
        Ok(())
    }

    /// An asset's record as stored: from its record pack, or from the flat
    /// file an older publish (or the segmented index) wrote.
    fn asset_record_bytes(&self, name: &str) -> Result<Vec<u8>> {
        use std::io::{Read as _, Seek as _};
        if let Some(at) = self.index.records.get(name) {
            let path = record_pack_path(&self.root, &at.pack);
            let mut f = std::fs::File::open(&path)
                .map_err(|_| StoreError::AssetNotFound(name.to_string()))?;
            f.seek(std::io::SeekFrom::Start(at.offset as u64))?;
            let mut bytes = vec![0u8; at.len as usize];
            f.read_exact(&mut bytes)?;
            return Ok(bytes);
        }
        let path = self.root.join("assets").join(format!("{name}.json"));
        std::fs::read(&path).map_err(|_| StoreError::AssetNotFound(name.to_string()))
    }

    /// Delete record packs no live asset points into. Records are small and
    /// a pack is immutable, so a replaced or unpublished asset leaves its
    /// bytes behind until the pack holds nothing live; `gc` sweeps those.
    fn sweep_dead_record_packs(&self) -> Result<u64> {
        let dir = record_packs_dir(&self.root);
        if !dir.is_dir() {
            return Ok(0);
        }
        let live: HashSet<[u8; 32]> = self.index.records.values().map(|r| r.pack).collect();
        let mut reclaimed = 0u64;
        for entry in std::fs::read_dir(&dir)?.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "cavsrec") {
                continue;
            }
            let hash = path.file_stem().and_then(|s| s.to_str()).and_then(from_hex);
            if hash.is_some_and(|h| live.contains(&h)) {
                continue;
            }
            reclaimed += entry.metadata().map(|m| m.len()).unwrap_or(0);
            let _ = std::fs::remove_file(&path);
        }
        Ok(reclaimed)
    }

    /// Unpublish an asset: drop its references (chunks may become zero-ref,
    /// reclaimable by `gc`). Returns false if the asset was not present.
    pub fn unpublish_asset(&mut self, name: &str) -> Result<bool> {
        let Some(chunks) = self.index.assets.remove(name) else {
            return Ok(false);
        };
        self.dirty_assets.insert(name.to_string());
        self.decrement(&chunks);
        self.index.records.remove(name);
        let path = self.root.join("assets").join(format!("{name}.json"));
        let _ = std::fs::remove_file(path);
        self.save_index()?;
        Ok(true)
    }

    fn decrement(&mut self, chunks: &[String]) {
        for hex in chunks {
            self.chunk_update(hex, |info| {
                info.refcount = info.refcount.saturating_sub(1);
                if info.refcount == 0 {
                    // Stamped 0 as a sentinel; real epoch set by caller-aware
                    // paths is unnecessary — gc uses now vs zero_since.
                    info.zero_since = Some(now_epoch());
                }
            });
        }
    }

    /// Remove chunks that have had refcount 0 for at least `grace_secs`.
    /// Returns (chunks removed, bytes reclaimed).
    ///
    /// Packfiles are immutable, so a packed chunk is only *logically*
    /// removed (its ledger entry disappears); the pack file itself is
    /// deleted — together with its sidecar index — once **no live ledger
    /// entry references it** (the roadmap's zero-live-pack policy; partial
    /// compaction is deliberately out of scope for 0.4.0).
    pub fn gc(&mut self, grace_secs: u64) -> Result<(u64, u64)> {
        let now = now_epoch();
        let doomed: Vec<String> = self
            .chunks_iter()
            .filter(|(_, i)| i.refcount == 0)
            .filter(|(_, i)| now.saturating_sub(i.zero_since.unwrap_or(0)) >= grace_secs)
            .map(|(h, _)| h)
            .collect();
        let mut bytes = 0u64;
        let mut touched_packs: HashSet<String> = HashSet::new();
        for hex in &doomed {
            if let Some(info) = self.chunk_remove(hex) {
                match info.pack {
                    Some(pack) => {
                        touched_packs.insert(pack);
                    }
                    None => {
                        bytes += info.len_stored as u64;
                        let _ = std::fs::remove_file(self.chunk_path(hex));
                    }
                }
            }
        }
        // Quarantine packs that no remaining chunk references (deleted only
        // after they also age out of quarantine, below).
        if !touched_packs.is_empty() {
            let live = self.live_pack_set();
            for pack in &touched_packs {
                if !live.contains(pack.as_str()) {
                    self.quarantine_pack(pack)?;
                }
            }
        }
        self.quarantine_orphan_packs(grace_secs)?;
        bytes += self.sweep_quarantine(grace_secs)?;
        // Bookkeeping, not content: not counted in what gc reports reclaimed.
        self.sweep_dead_record_packs()?;
        self.save_index()?;
        Ok((doomed.len() as u64, bytes))
    }

    fn quarantine_dir(&self) -> PathBuf {
        self.root.join("quarantine")
    }

    /// Move a pack (and its sidecar index) out of the live tree into
    /// `quarantine/`, stamping when it got there. Quarantined packs are
    /// still recoverable: opening a store whose ledger references one moves
    /// it straight back (see [`Self::restore_quarantined_packs`]); only
    /// [`Self::sweep_quarantine`] deletes, and only after the pack has also
    /// sat out the quarantine period. Two-stage deletion means an eventual-
    /// consistency or in-flight-finalize race costs a restore, not data.
    fn quarantine_pack(&self, hex: &str) -> Result<()> {
        let qdir = self.quarantine_dir();
        std::fs::create_dir_all(&qdir)?;
        let src = packfile::pack_path(&self.packs_dir(), hex);
        if src.exists() {
            std::fs::rename(&src, qdir.join(format!("{hex}.cavspack")))?;
        }
        let idx = packfile::index_path(&self.packs_dir(), hex);
        if idx.exists() {
            std::fs::rename(&idx, qdir.join(format!("{hex}.cavsindex")))?;
        }
        std::fs::write(qdir.join(format!("{hex}.qsince")), now_epoch().to_string())?;
        Ok(())
    }

    /// Delete quarantined packs that have sat in quarantine for at least
    /// `quarantine_secs`. A pack the ledger references again (it was
    /// quarantined by mistake or restored logically) is moved back instead
    /// of deleted. Returns bytes reclaimed.
    fn sweep_quarantine(&self, quarantine_secs: u64) -> Result<u64> {
        let qdir = self.quarantine_dir();
        if !qdir.is_dir() {
            return Ok(0);
        }
        let live = self.live_pack_set();
        let now = now_epoch();
        let mut bytes = 0u64;
        for entry in std::fs::read_dir(&qdir)?.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "cavspack") {
                continue;
            }
            let Some(hex) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
                continue;
            };
            if live.contains(hex.as_str()) {
                self.restore_pack_from_quarantine(&hex)?;
                continue;
            }
            let since = std::fs::read_to_string(qdir.join(format!("{hex}.qsince")))
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok());
            let Some(since) = since else {
                // Missing/unreadable stamp: restart the clock, never delete
                // on unknown age.
                std::fs::write(qdir.join(format!("{hex}.qsince")), now.to_string())?;
                continue;
            };
            if now.saturating_sub(since) < quarantine_secs {
                continue;
            }
            if let Ok(meta) = std::fs::metadata(&path) {
                bytes += meta.len();
            }
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(qdir.join(format!("{hex}.cavsindex")));
            let _ = std::fs::remove_file(qdir.join(format!("{hex}.qsince")));
        }
        Ok(bytes)
    }

    /// Move a quarantined pack back into the live tree.
    fn restore_pack_from_quarantine(&self, hex: &str) -> Result<()> {
        let qdir = self.quarantine_dir();
        let dst = packfile::pack_path(&self.packs_dir(), hex);
        std::fs::create_dir_all(dst.parent().unwrap())?;
        let src = qdir.join(format!("{hex}.cavspack"));
        if src.exists() && !dst.exists() {
            std::fs::rename(&src, &dst)?;
        }
        let qidx = qdir.join(format!("{hex}.cavsindex"));
        if qidx.exists() {
            let idst = packfile::index_path(&self.packs_dir(), hex);
            if !idst.exists() {
                std::fs::rename(&qidx, &idst)?;
            }
        }
        let _ = std::fs::remove_file(qdir.join(format!("{hex}.qsince")));
        Ok(())
    }

    /// On open: any quarantined pack the ledger still references goes back
    /// into the live tree (e.g. the ledger was recovered from
    /// `index.bin.prev`, or a GC raced a finalize).
    fn restore_quarantined_packs(&self) -> Result<()> {
        let qdir = self.quarantine_dir();
        if !qdir.is_dir() {
            return Ok(());
        }
        let live = self.live_pack_set();
        for entry in std::fs::read_dir(&qdir)?.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "cavspack") {
                continue;
            }
            if let Some(hex) = path.file_stem().and_then(|s| s.to_str()) {
                if live.contains(hex) {
                    self.restore_pack_from_quarantine(hex)?;
                }
            }
        }
        Ok(())
    }

    /// Quarantine sealed packs on disk that no ledger chunk references —
    /// the residue of a session that flushed a pack (rollover) but died
    /// before committing its publish batch. Such packs are invisible to the
    /// refcount path above (no ledger entry ever pointed at them). The same
    /// `grace_secs` applies, against the pack's mtime, so a concurrent
    /// ingest's freshly sealed-but-not-yet-committed pack is never touched
    /// by an aggressive `gc(0)` from another process. Deletion happens only
    /// later, in [`Self::sweep_quarantine`].
    fn quarantine_orphan_packs(&self, grace_secs: u64) -> Result<()> {
        let packs_dir = self.packs_dir();
        if !packs_dir.is_dir() {
            return Ok(());
        }
        let live = self.live_pack_set();
        let now = std::time::SystemTime::now();
        for shard in std::fs::read_dir(&packs_dir)?.flatten() {
            if !shard.path().is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(shard.path())?.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "cavspack") {
                    continue;
                }
                let Some(hex) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if live.contains(hex) {
                    continue;
                }
                let old_enough = entry.metadata().and_then(|m| m.modified()).is_ok_and(|m| {
                    now.duration_since(m)
                        .is_ok_and(|age| age.as_secs() >= grace_secs)
                });
                if !old_enough {
                    continue;
                }
                self.quarantine_pack(hex)?;
            }
        }
        Ok(())
    }

    pub fn asset_names(&self) -> Vec<String> {
        self.index.assets.keys().cloned().collect()
    }

    /// One asset's physical footprint: total stored (compressed) bytes and
    /// chunk count across the chunks it references. Shared chunks are counted
    /// for each asset that references them, so this is the object's *standalone*
    /// compressed size — useful for per-object stats. Summing it across assets
    /// over-counts cross-asset dedup; the repo's true physical is the unique
    /// packed bytes, not this sum. Returns `None` for an unknown asset.
    pub fn asset_stored_stats(&self, name: &str) -> Option<(u64, u64)> {
        let hexes = self.index.assets.get(name)?;
        let mut stored = 0u64;
        let mut chunks = 0u64;
        for hex in hexes {
            if let Some(info) = self.chunk_get(hex) {
                stored += info.len_stored as u64;
                chunks += 1;
            }
        }
        Some((stored, chunks))
    }

    pub fn get_asset(&self, name: &str) -> Result<AssetRecord> {
        Ok(serde_json::from_slice(&self.asset_record_bytes(name)?)?)
    }

    pub fn stats(&self) -> StoreStats {
        // One streaming pass over the ledger (in segmented mode this walks
        // the mmaps without materializing the table).
        let mut unique_chunks = 0u64;
        let mut stored_bytes = 0u64;
        let mut unique_raw_bytes = 0u64;
        let mut zero_ref_chunks = 0u64;
        let mut pack_ids: HashSet<String> = HashSet::new();
        let mut pack_live_bytes = 0u64;
        for (_, info) in self.chunks_iter() {
            unique_chunks += 1;
            stored_bytes += info.len_stored as u64;
            unique_raw_bytes += info.len_raw as u64;
            if info.refcount == 0 {
                zero_ref_chunks += 1;
            }
            if let Some(pack) = info.pack {
                pack_ids.insert(pack);
                pack_live_bytes += info.len_stored as u64;
            }
        }
        // Logical = if every asset stored its own copy of every chunk.
        let mut logical = 0u64;
        for chunks in self.index.assets.values() {
            for hex in chunks {
                if let Some(i) = self.chunk_get(hex) {
                    logical += i.len_stored as u64;
                }
            }
        }
        let pack_disk_bytes: u64 = pack_ids
            .iter()
            .filter_map(|p| std::fs::metadata(packfile::pack_path(&self.packs_dir(), p)).ok())
            .map(|m| m.len())
            .sum();
        StoreStats {
            assets: self.index.assets.len(),
            unique_chunks,
            stored_bytes,
            unique_raw_bytes,
            logical_stored_bytes: logical,
            zero_ref_chunks,
            layout: self.index.layout,
            pack_count: pack_ids.len() as u64,
            pack_disk_bytes,
            pack_live_bytes,
        }
    }

    /// Verify: every ledger chunk reads back (loose file or pack range),
    /// decompresses when stored with zstd (undoing the BG4 pretransform when
    /// flagged), and re-hashes to its identity; every referenced pack passes
    /// its header/footer check. Returns the number of chunks checked.
    pub fn verify(&self) -> Result<u64> {
        // Cap decompression by the ledger's own raw length, itself sane-
        // bounded so a corrupt ledger cannot request a huge allocation.
        const MAX_RAW: u64 = 256 * 1024 * 1024;
        let mut checked = 0u64;
        for (hex, _) in self.chunks_iter() {
            let hex = &hex;
            checked += 1;
            let hash = from_hex(hex).ok_or_else(|| StoreError::BadHash(hex.clone()))?;
            let (stored, flags, len_raw) = self.read_chunk_stored(&hash)?;
            let mut raw = if flags & 1 != 0 {
                // CHUNK_FLAG_ZSTD == 1 (cavs-format), kept as a plain bit
                // here to avoid a dependency cycle.
                if len_raw as u64 > MAX_RAW {
                    return Err(StoreError::BadHash(format!("{hex}: raw length too large")));
                }
                zstd::bulk::decompress(&stored, len_raw as usize)
                    .map_err(|e| StoreError::BadHash(format!("{hex}: zstd: {e}")))?
            } else {
                stored
            };
            if flags & 2 != 0 {
                // CHUNK_FLAG_BG4 == 2 (cavs-format): undo the byte-grouping
                // pretransform before re-hashing.
                raw = bg4_ungroup(&raw);
            }
            if raw.len() != len_raw as usize || cavs_hash::hash_chunk(&raw) != hash {
                return Err(StoreError::BadHash(hex.clone()));
            }
        }
        for pack in self.live_pack_set() {
            packfile::verify_pack(&packfile::pack_path(&self.packs_dir(), &pack))?;
        }
        // Segmented mode: the index's own per-segment seals too.
        if let Some(seg) = &self.seg {
            seg.index.verify_segments()?;
        }
        Ok(checked)
    }

    /// Export the store as a deterministic, immutable object tree ready to
    /// upload to object storage / a CDN:
    ///
    /// ```text
    /// out/
    ///   chunks/packs/<ab>/<id>.cavspack     immutable (content-addressed)
    ///   chunks/indexes/<ab>/<id>.cavsindex  immutable
    ///   assets/<name>/record.json           mutable per release
    /// ```
    ///
    /// Requires the packfile layout with every live chunk packed. Returns
    /// the relative paths written, packs first.
    pub fn export_object_store(&self, out: &Path) -> Result<Vec<String>> {
        if self.index.layout != StoreLayout::Packfiles {
            return Err(StoreError::NotExportable(
                "object-store export requires a packfile-layout store".into(),
            ));
        }
        if let Some((hex, _)) = self.chunks_iter().find(|(_, i)| i.pack.is_none()) {
            return Err(StoreError::NotExportable(format!(
                "chunk {hex} is not packed (ingest still open?)"
            )));
        }
        let mut written = Vec::new();
        let mut packs: Vec<String> = self.live_pack_set().into_iter().collect();
        packs.sort_unstable();
        for pack in packs {
            for (src, rel) in [
                (
                    packfile::pack_path(&self.packs_dir(), &pack),
                    format!("chunks/packs/{}/{pack}.cavspack", &pack[..2]),
                ),
                (
                    packfile::index_path(&self.packs_dir(), &pack),
                    format!("chunks/indexes/{}/{pack}.cavsindex", &pack[..2]),
                ),
            ] {
                copy_if_different(&src, &out.join(&rel))?;
                written.push(rel);
            }
        }
        for name in self.index.assets.keys() {
            let rel = format!("assets/{name}/record.json");
            let dst = out.join(&rel);
            std::fs::create_dir_all(dst.parent().unwrap())?;
            std::fs::write(&dst, self.asset_record_bytes(name)?)?;
            written.push(rel);
        }
        Ok(written)
    }

    /// v0.6.0 static/CDN compatibility: write one `chunk-map.json` per
    /// asset into an exported object tree. It maps every chunk the asset
    /// references to its immutable pack file and byte range, so a client
    /// against a *static* HTTP host can plan a fetch (compute its missing
    /// set, then issue pack range requests) with no smart server at all.
    pub fn export_static_plans(&self, out: &Path) -> Result<Vec<String>> {
        if self.index.layout != StoreLayout::Packfiles {
            return Err(StoreError::NotExportable(
                "static plans require a packfile-layout store".into(),
            ));
        }
        let mut written = Vec::new();
        for name in self.index.assets.keys() {
            written.push(self.write_chunk_map(name, out)?);
        }
        Ok(written)
    }

    /// The chunk-map entries of one asset (every chunk it references,
    /// mapped to its immutable pack file and byte range), as published in
    /// `chunk-map.json` and in session meta-packs.
    fn chunk_map_entries(&self, name: &str) -> Result<Vec<serde_json::Value>> {
        let hexes = self
            .index
            .assets
            .get(name)
            .ok_or_else(|| StoreError::AssetNotFound(name.to_string()))?;
        let mut chunks = Vec::with_capacity(hexes.len());
        for hex in hexes {
            let Some(info) = self.chunk_get(hex) else {
                continue;
            };
            let Some(pack) = info.pack.as_deref() else {
                return Err(StoreError::NotExportable(format!(
                    "chunk {hex} is not packed (ingest still open?)"
                )));
            };
            // `pack_offset` is into the pack's data region; a static
            // client that knows nothing about the packfile header wants
            // the absolute file offset for its HTTP Range request, so we
            // publish both.
            let pack_offset = info.pack_offset.unwrap_or(0);
            chunks.push(serde_json::json!({
                "hash": hex,
                "len_raw": info.len_raw,
                "len_stored": info.len_stored,
                "flags": info.flags,
                "pack": format!("chunks/packs/{}/{pack}.cavspack", &pack[..2]),
                "pack_offset": pack_offset,
                "pack_offset_abs": packfile::PACK_HEADER_LEN + pack_offset,
            }));
        }
        Ok(chunks)
    }

    /// Chunk-map **v2 by runs** (Round 3B): the same information as
    /// [`Self::chunk_map_entries`], but physically contiguous chunks of the
    /// same pack collapse into one run — the pack path and start offset are
    /// stated once and per-chunk offsets are implicit (cumulative
    /// `len_stored`). A push writes an object's chunks contiguously, so a
    /// many-chunk object typically serializes as a handful of runs instead
    /// of one verbose entry per chunk, cutting metadata bytes well past the
    /// 30% target. `flags` collapses to a single integer when uniform
    /// across the run (the common case).
    ///
    /// Run shape:
    /// ```json
    /// {"pack": "chunks/packs/ab/<id>.cavspack", "start_abs": 16,
    ///  "hashes": ["..."], "lens_raw": [..], "lens_stored": [..],
    ///  "flags": 3 }
    /// ```
    fn chunk_map_runs(&self, name: &str) -> Result<Vec<serde_json::Value>> {
        let hexes = self
            .index
            .assets
            .get(name)
            .ok_or_else(|| StoreError::AssetNotFound(name.to_string()))?;
        // Order by physical position so contiguity is visible.
        let mut placed: Vec<(String, ChunkInfo)> = Vec::with_capacity(hexes.len());
        for hex in hexes {
            let Some(info) = self.chunk_get(hex) else {
                continue;
            };
            if info.pack.is_none() {
                return Err(StoreError::NotExportable(format!(
                    "chunk {hex} is not packed (ingest still open?)"
                )));
            }
            placed.push((hex.clone(), info));
        }
        placed.sort_by(|a, b| {
            (a.1.pack.as_deref(), a.1.pack_offset).cmp(&(b.1.pack.as_deref(), b.1.pack_offset))
        });

        struct Run {
            pack: String,
            start_abs: u64,
            next_offset: u64,
            hashes: Vec<String>,
            lens_raw: Vec<u32>,
            lens_stored: Vec<u32>,
            flags: Vec<u32>,
        }
        let mut runs: Vec<Run> = Vec::new();
        for (hex, info) in placed {
            let pack = info.pack.as_deref().unwrap();
            let offset = info.pack_offset.unwrap_or(0);
            let extend = runs
                .last()
                .is_some_and(|r: &Run| r.pack == pack && offset == r.next_offset);
            if !extend {
                runs.push(Run {
                    pack: pack.to_string(),
                    start_abs: packfile::PACK_HEADER_LEN + offset,
                    next_offset: offset,
                    hashes: Vec::new(),
                    lens_raw: Vec::new(),
                    lens_stored: Vec::new(),
                    flags: Vec::new(),
                });
            }
            let run = runs.last_mut().unwrap();
            run.next_offset = offset + info.len_stored as u64;
            run.hashes.push(hex);
            run.lens_raw.push(info.len_raw);
            run.lens_stored.push(info.len_stored);
            run.flags.push(info.flags);
        }

        Ok(runs
            .into_iter()
            .map(|r| {
                let uniform = r.flags.windows(2).all(|w| w[0] == w[1]);
                let flags: serde_json::Value = if uniform {
                    r.flags.first().copied().unwrap_or(0).into()
                } else {
                    r.flags.into()
                };
                serde_json::json!({
                    "pack": format!("chunks/packs/{}/{}.cavspack", &r.pack[..2], r.pack),
                    "start_abs": r.start_abs,
                    "hashes": r.hashes,
                    "lens_raw": r.lens_raw,
                    "lens_stored": r.lens_stored,
                    "flags": flags,
                })
            })
            .collect())
    }

    /// Write `assets/<name>/chunk-map.json` for one asset; returns the
    /// relative path written.
    fn write_chunk_map(&self, name: &str, out: &Path) -> Result<String> {
        let chunks = self.chunk_map_entries(name)?;
        let rel = format!("assets/{name}/chunk-map.json");
        let dst = out.join(&rel);
        std::fs::create_dir_all(dst.parent().unwrap())?;
        std::fs::write(
            &dst,
            serde_json::to_vec_pretty(&serde_json::json!({
                "asset": name,
                "chunks": chunks,
            }))?,
        )?;
        Ok(rel)
    }

    /// Incrementally export **one asset** into an export tree: the packs it
    /// references (skipped when already present), its `record.json`,
    /// `chunk-map.json` and `manifest.json`. Equivalent, for that asset, to
    /// the full `export_object_store` + `export_static_plans` +
    /// [`Self::export_static_manifests`] — but O(this asset), not O(store),
    /// so per-object publishers (e.g. the Git LFS agent) stay linear across
    /// a many-object push.
    pub fn export_asset(&self, name: &str, out: &Path) -> Result<Vec<String>> {
        if self.index.layout != StoreLayout::Packfiles {
            return Err(StoreError::NotExportable(
                "object-store export requires a packfile-layout store".into(),
            ));
        }
        let hexes = self
            .index
            .assets
            .get(name)
            .ok_or_else(|| StoreError::AssetNotFound(name.to_string()))?;
        let mut packs: Vec<String> = Vec::new();
        for hex in hexes {
            let Some(info) = self.chunk_get(hex) else {
                continue;
            };
            match info.pack {
                Some(pack) => {
                    if !packs.contains(&pack) {
                        packs.push(pack);
                    }
                }
                None => {
                    return Err(StoreError::NotExportable(format!(
                        "chunk {hex} is not packed (ingest still open?)"
                    )))
                }
            }
        }
        packs.sort_unstable();

        let mut written = Vec::new();
        for pack in packs {
            for (src, rel) in [
                (
                    packfile::pack_path(&self.packs_dir(), &pack),
                    format!("chunks/packs/{}/{pack}.cavspack", &pack[..2]),
                ),
                (
                    packfile::index_path(&self.packs_dir(), &pack),
                    format!("chunks/indexes/{}/{pack}.cavsindex", &pack[..2]),
                ),
            ] {
                if copy_if_different(&src, &out.join(&rel))? {
                    written.push(rel);
                }
            }
        }

        let rel = format!("assets/{name}/record.json");
        let dst = out.join(&rel);
        std::fs::create_dir_all(dst.parent().unwrap())?;
        std::fs::write(&dst, self.asset_record_bytes(name)?)?;
        written.push(rel);

        written.push(self.write_chunk_map(name, out)?);

        let manifest = self.asset_manifest(name)?;
        let rel = format!("assets/{name}/manifest.json");
        let dst = out.join(&rel);
        std::fs::create_dir_all(dst.parent().unwrap())?;
        std::fs::write(&dst, serde_json::to_vec_pretty(&manifest)?)?;
        written.push(rel);

        Ok(written)
    }

    /// Round 3A: publish one **session meta-pack** into the export tree —
    /// a single zstd-compressed artifact carrying the manifest + chunk-map
    /// of every asset in `names` — and update `meta/index.json` (oid →
    /// pack). A client resolving any one of these assets downloads the
    /// pack once and has the metadata of every sibling of the push, so a
    /// many-object clone spends a handful of metadata round-trips instead
    /// of two per object.
    ///
    /// The pack is content-addressed (BLAKE3 of its bytes) and immutable;
    /// the index is rewritten atomically and, when unreadable, rebuilt by
    /// scanning the packs themselves. Returns the new pack's id, or `None`
    /// when `names` is empty.
    pub fn export_meta_pack(&self, names: &[String], out: &Path) -> Result<Option<String>> {
        if names.is_empty() {
            return Ok(None);
        }
        let mut objects = Vec::with_capacity(names.len());
        for name in names {
            // Locations travel as v2 runs; readers that predate runs fall
            // back to the per-asset chunk-map.json (still v1) on their own.
            objects.push(serde_json::json!({
                "oid": name,
                "manifest": self.asset_manifest(name)?,
                "runs": self.chunk_map_runs(name)?,
            }));
        }
        let raw = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "objects": objects,
        }))?;
        let compressed = zstd::bulk::compress(&raw, 9)
            .map_err(|e| StoreError::NotExportable(format!("compressing meta-pack: {e}")))?;
        let id = cavs_hash::to_hex(&cavs_hash::hash_chunk(&compressed));

        let packs_dir = out.join("meta").join("packs");
        std::fs::create_dir_all(&packs_dir)?;
        let dst = packs_dir.join(format!("{id}.cmeta"));
        if !dst.exists() {
            let tmp = packs_dir.join(format!("{id}.cmeta.tmp"));
            std::fs::write(&tmp, &compressed)?;
            std::fs::rename(&tmp, &dst)?;
        }

        // Update the oid → pack index: append this pack, atomically.
        let index_path = out.join("meta").join("index.json");
        let mut index = read_or_rebuild_meta_index(&index_path, &packs_dir);
        index.retain(|p| p.id != id);
        index.push(MetaIndexEntry {
            id: id.clone(),
            oids: names.to_vec(),
        });
        let generation = 1 + index.len() as u64;
        let doc = serde_json::json!({
            "version": 1,
            "generation": generation,
            "packs": index
                .iter()
                .map(|p| serde_json::json!({ "id": p.id, "oids": p.oids }))
                .collect::<Vec<_>>(),
        });
        let tmp = index_path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&doc)?)?;
        std::fs::rename(&tmp, &index_path)?;
        Ok(Some(id))
    }

    /// Build the runtime [`cavs_proto::Manifest`] for a stored asset (the
    /// reconstruction structure a client needs: ordered chunks per
    /// track/segment, with each chunk's raw length pulled from the store
    /// ledger). Mirrors the server's `AppState::manifest`, but reads from an
    /// [`AssetRecord`] + the chunk ledger so a *serverless* client can plan a
    /// fetch from a static export.
    pub fn asset_manifest(&self, name: &str) -> Result<cavs_proto::Manifest> {
        let record = self.get_asset(name)?;
        let chunk_ref = |hex: &str| {
            let len = from_hex(hex)
                .and_then(|h| self.chunk_info(&h))
                .map(|i| i.len_raw)
                .unwrap_or(0);
            cavs_proto::ChunkRef {
                hash: hex.to_string(),
                len,
            }
        };
        // Track kind labels as encoded by the `.cavs` container (see
        // `cavs_format::TrackKind`); re-stated locally because cavs-format
        // depends on this crate, so we cannot depend on it back.
        let kind_label = |kind: u8| match kind {
            0 => "video",
            1 => "audio",
            2 => "subtitle",
            _ => "data",
        };
        Ok(cavs_proto::Manifest {
            asset: record.name.clone(),
            asset_uuid: record.asset_uuid.clone(),
            tracks: record
                .tracks
                .iter()
                .map(|t| cavs_proto::ManifestTrack {
                    track_id: t.track_id,
                    kind: kind_label(t.kind).to_string(),
                    codec: t.codec.clone(),
                    name: t.name.clone(),
                    timescale: t.timescale,
                    init_chunks: t.init_chunks.iter().map(|h| chunk_ref(h)).collect(),
                })
                .collect(),
            segments: record
                .segments
                .iter()
                .map(|s| cavs_proto::ManifestSegment {
                    segment_id: s.segment_id,
                    track_id: s.track_id,
                    pts_start: s.pts_start,
                    duration: s.duration,
                    random_access: s.random_access,
                    chunks: s.chunks.iter().map(|h| chunk_ref(h)).collect(),
                })
                .collect(),
            dict: record.dict.clone(),
            chunk_table: record.chunk_table.clone(),
            merkle_root: record.merkle_root.clone(),
            signature: record.signature.clone(),
            signer_pubkey: record.signer_pubkey.clone(),
            meta: record.meta.clone(),
        })
    }

    /// Write `assets/<name>/manifest.json` for every asset into an export
    /// tree, so a serverless client can read the reconstruction structure
    /// with no running server. Returns the relative paths written.
    pub fn export_static_manifests(&self, out: &Path) -> Result<Vec<String>> {
        let mut written = Vec::new();
        for name in self.asset_names() {
            let manifest = self.asset_manifest(&name)?;
            let rel = format!("assets/{name}/manifest.json");
            let dst = out.join(&rel);
            std::fs::create_dir_all(dst.parent().unwrap())?;
            std::fs::write(&dst, serde_json::to_vec_pretty(&manifest)?)?;
            written.push(rel);
        }
        Ok(written)
    }

    /// Persist the ledger crash-safely. The snapshot is staged to a temp
    /// file, fsynced, read back and seal-verified before it replaces
    /// `index.bin`; the outgoing snapshot is kept one generation as
    /// `index.bin.prev` (the open path falls back to it). At no point does
    /// a readable `index.bin`/`index.bin.prev` pair not exist, so a crash
    /// anywhere in this sequence loses at most the in-memory batch, never
    /// the store.
    fn save_index(&mut self) -> Result<()> {
        // Segmented mode: the overlay becomes one delta segment and a new
        // generation — the ledger is never rewritten whole.
        if let Some(seg) = &mut self.seg {
            let overlay = std::mem::take(&mut seg.overlay);
            seg.index.commit_generation(&overlay, &self.index.assets)?;
            self.index.generation = seg.index.generation;
            return Ok(());
        }
        // Monolithic mode: a save is a journal record of what it touched.
        // The snapshot is rewritten only when the journal has outgrown it —
        // past that point a fresh snapshot is cheaper to replay than the
        // records it would replace — or when there is no snapshot to extend.
        let generation = self.index.generation + 1;
        let dirty_chunks = std::mem::take(&mut self.dirty_chunks);
        let dirty_assets = std::mem::take(&mut self.dirty_assets);
        if self.snapshot_bytes > 0 {
            let record =
                encode_journal_record(&self.index, generation, &dirty_chunks, &dirty_assets);
            if self.journal_bytes + record.len() as u64 <= self.journal_budget() {
                if let Err(e) = self.append_journal(&record) {
                    // Nothing landed: the entries are still dirty and the
                    // next save carries them.
                    self.dirty_chunks = dirty_chunks;
                    self.dirty_assets = dirty_assets;
                    return Err(e);
                }
                self.index.generation = generation;
                self.journal_bytes += record.len() as u64;
                return Ok(());
            }
        }
        self.index.generation = generation;
        self.write_snapshot()?;
        Ok(())
    }

    /// Append one sealed record to `index.log`, durably. The journal is
    /// created on the first append; a file that already exists is extended
    /// in place, which is one fsync and no directory update.
    fn append_journal(&mut self, record: &[u8]) -> Result<()> {
        use std::io::Write as _;
        let path = self.root.join(JOURNAL_FILE);
        let created = self.journal_bytes == 0;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)?;
        let written = f.write_all(record).and_then(|_| f.sync_all());
        if let Err(e) = written {
            // A partial record would stop every later replay at this
            // offset; cut back to the last record boundary before failing.
            let _ = f.set_len(self.journal_bytes);
            return Err(e.into());
        }
        if created {
            if let Ok(dir) = std::fs::File::open(&self.root) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    }

    /// Write the whole ledger as a fresh `index.bin`, keeping the previous
    /// snapshot as `index.bin.prev`, and rotate the journal it supersedes to
    /// `index.log.prev` so a recovery from `.prev` still has the saves
    /// between the two snapshots.
    fn write_snapshot(&mut self) -> Result<()> {
        let path = self.root.join("index.bin");
        let prev = self.root.join("index.bin.prev");
        let tmp = path.with_extension("bin.tmp");
        let encoded = encode_index(&self.index);
        {
            use std::io::Write as _;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&encoded)?;
            f.sync_all()?;
        }
        // Read back what the filesystem actually holds and check the seal:
        // a truncated or bit-flipped staging write must fail here, not at
        // the next open. The seal is what decoding would check first, and
        // it costs a hash where a decode costs a ledger's worth of
        // allocations.
        if !index_seal_holds(&std::fs::read(&tmp)?) {
            let _ = std::fs::remove_file(&tmp);
            return Err(StoreError::IndexCorrupt(
                "snapshot read back from disk does not match what was written".into(),
            ));
        }
        if path.exists() {
            std::fs::rename(&path, &prev)?;
        }
        std::fs::rename(&tmp, &path)?;
        let log = self.root.join(JOURNAL_FILE);
        let log_prev = self.root.join(JOURNAL_PREV_FILE);
        if log.exists() {
            std::fs::rename(&log, &log_prev)?;
        } else {
            // Two snapshots in a row: the older rotated journal predates
            // `index.bin.prev` and no recovery can use it.
            let _ = std::fs::remove_file(&log_prev);
        }
        // Make the renames durable before reporting success.
        if let Ok(dir) = std::fs::File::open(&self.root) {
            let _ = dir.sync_all();
        }
        // A legacy pre-1.6 ledger is superseded by this save; leaving it
        // behind would resurrect stale state on a downgrade mid-history.
        let _ = std::fs::remove_file(self.root.join("index.json"));
        self.snapshot_bytes = encoded.len() as u64;
        self.journal_bytes = 0;
        Ok(())
    }
}

/// Where a loaded ledger came from; only the live snapshot is something the
/// journal can extend.
enum LedgerSource {
    /// `index.bin`, with its size.
    Live(u64),
    /// `index.bin.prev`: the live snapshot was missing or corrupt.
    Prev,
    /// A v1 `index.bin`, written before the journal existed.
    Legacy,
    /// A pre-1.6 `index.json`.
    Json,
}

/// Write an asset's record file (`assets/<name>.json`) atomically.
fn write_asset_record_at(root: &Path, record: &AssetRecord) -> Result<()> {
    let json = serde_json::to_vec_pretty(record)?;
    let path = root.join("assets").join(format!("{}.json", record.name));
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Where record packs live, and the path of one.
fn record_packs_dir(root: &Path) -> PathBuf {
    root.join("assets").join("records")
}

fn record_pack_path(root: &Path, pack: &[u8; 32]) -> PathBuf {
    record_packs_dir(root).join(format!("{}.cavsrec", to_hex(pack)))
}

/// Write one record pack holding `records` back to back, and say where each
/// landed. The file is named by the BLAKE3 of its bytes, so it is immutable
/// once written and a batch published twice is one file. Synced before it is
/// named: the ledger that points into it is synced right after, and a record
/// the ledger names must be there to read.
fn write_record_pack(root: &Path, records: &[AssetRecord]) -> Result<Vec<(String, RecordRef)>> {
    use std::io::Write as _;
    let mut bytes = Vec::new();
    let mut spans: Vec<(String, u32, u32)> = Vec::with_capacity(records.len());
    for record in records {
        let json = serde_json::to_vec_pretty(record)?;
        spans.push((record.name.clone(), bytes.len() as u32, json.len() as u32));
        bytes.extend_from_slice(&json);
    }
    let pack = cavs_hash::hash_chunk(&bytes);
    let path = record_pack_path(root, &pack);
    if !path.exists() {
        std::fs::create_dir_all(path.parent().unwrap())?;
        let tmp = path.with_extension("cavsrec.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &path)?;
    }
    Ok(spans
        .into_iter()
        .map(|(name, offset, len)| (name, RecordRef { pack, offset, len }))
        .collect())
}

/// Write a batch of flat asset records (`assets/<name>.json`), across threads
/// once there are enough to pay for them. Each record is four filesystem
/// calls in a directory that holds every asset the store has, and they wait
/// on the disk rather than on each other.
fn write_flat_asset_records(root: &Path, records: &[AssetRecord]) -> Result<()> {
    const PER_THREAD: usize = 32;
    const MAX_THREADS: usize = 8;
    if records.len() <= PER_THREAD {
        return records
            .iter()
            .try_for_each(|r| write_asset_record_at(root, r));
    }
    let threads = records.len().div_ceil(PER_THREAD).min(MAX_THREADS);
    let per = records.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let workers: Vec<_> = records
            .chunks(per)
            .map(|part| {
                scope.spawn(move || part.iter().try_for_each(|r| write_asset_record_at(root, r)))
            })
            .collect();
        for worker in workers {
            worker.join().expect("asset record writer panicked")?;
        }
        Ok(())
    })
}

// --- ledger journal (index.log) --------------------------------------------
//
// A save in monolithic mode appends one record describing the entries it
// touched, sealed on its own so a torn tail is detected at the record where
// it happened. Layout, little-endian throughout:
//
//   "CAVSIDL1"        magic
//   u16 version       readers reject versions above their own
//   u16 reserved      0
//   u32 body_len
//   u64 generation    the ledger generation this record produces
//   body:
//     u16 pack_count  { u16 len, hex bytes } × pack_count
//     u32 upserts     { hash 32B, len_raw u32, len_stored u32, flags u32,
//                       refcount u64, zero_since u64 (MAX = none),
//                       pack_ord u16 (MAX = none), pack_offset u64 } × upserts
//     u32 removals    { hash 32B } × removals
//     u32 asset_puts  { u16 len, name bytes, u32 n, hash 32B × n,
//                       u8 packed, if 1: pack 32B, u32 offset, u32 len } × asset_puts
//     u32 asset_dels  { u16 len, name bytes } × asset_dels
//   BLAKE3 of everything above (32B seal)
//
// Replay applies records in generation order, each exactly one past the
// ledger it lands on: an older record is a rotated journal's and is skipped,
// a gap or a bad seal ends the replay where it stands.

const JOURNAL_FILE: &str = "index.log";
const JOURNAL_PREV_FILE: &str = "index.log.prev";
const JOURNAL_MAGIC: &[u8; 8] = b"CAVSIDL1";
const JOURNAL_VERSION: u16 = 1;
const JOURNAL_HEADER_SIZE: usize = 24;
const JOURNAL_SEAL_SIZE: usize = 32;
/// The journal may always grow to this before a snapshot is rewritten, so a
/// small store is not snapshotting on every other save.
pub const JOURNAL_MIN_BYTES: u64 = 1 << 20;

fn encode_journal_record(
    index: &Index,
    generation: u64,
    dirty_chunks: &BTreeSet<String>,
    dirty_assets: &BTreeSet<String>,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        JOURNAL_HEADER_SIZE
            + JOURNAL_SEAL_SIZE
            + dirty_chunks.len() * 72
            + dirty_assets.len() * 128,
    );
    out.extend_from_slice(JOURNAL_MAGIC);
    out.extend_from_slice(&JOURNAL_VERSION.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // body_len, patched below
    out.extend_from_slice(&generation.to_le_bytes());
    debug_assert_eq!(out.len(), JOURNAL_HEADER_SIZE);

    let mut upserts: Vec<(&str, &ChunkInfo)> = Vec::new();
    let mut removals: Vec<&str> = Vec::new();
    for hex in dirty_chunks {
        match index.chunks.get(hex) {
            Some(info) => upserts.push((hex, info)),
            None => removals.push(hex),
        }
    }
    let mut packs: Vec<&str> = Vec::new();
    let mut pack_ord: HashMap<&str, u16> = HashMap::new();
    for (_, info) in &upserts {
        if let Some(p) = info.pack.as_deref() {
            if !pack_ord.contains_key(p) {
                pack_ord.insert(p, packs.len() as u16);
                packs.push(p);
            }
        }
    }
    out.extend_from_slice(&(packs.len() as u16).to_le_bytes());
    for p in &packs {
        out.extend_from_slice(&(p.len() as u16).to_le_bytes());
        out.extend_from_slice(p.as_bytes());
    }
    out.extend_from_slice(&(upserts.len() as u32).to_le_bytes());
    for (hex, info) in &upserts {
        out.extend_from_slice(&from_hex(hex).unwrap_or([0u8; 32]));
        out.extend_from_slice(&info.len_raw.to_le_bytes());
        out.extend_from_slice(&info.len_stored.to_le_bytes());
        out.extend_from_slice(&info.flags.to_le_bytes());
        out.extend_from_slice(&info.refcount.to_le_bytes());
        out.extend_from_slice(&info.zero_since.unwrap_or(u64::MAX).to_le_bytes());
        let ord = info
            .pack
            .as_deref()
            .and_then(|p| pack_ord.get(p).copied())
            .unwrap_or(u16::MAX);
        out.extend_from_slice(&ord.to_le_bytes());
        out.extend_from_slice(&info.pack_offset.unwrap_or(0).to_le_bytes());
    }
    out.extend_from_slice(&(removals.len() as u32).to_le_bytes());
    for hex in &removals {
        out.extend_from_slice(&from_hex(hex).unwrap_or([0u8; 32]));
    }

    let mut asset_puts: Vec<(&str, &Vec<String>)> = Vec::new();
    let mut asset_dels: Vec<&str> = Vec::new();
    for name in dirty_assets {
        match index.assets.get(name) {
            Some(chunks) => asset_puts.push((name, chunks)),
            None => asset_dels.push(name),
        }
    }
    out.extend_from_slice(&(asset_puts.len() as u32).to_le_bytes());
    for (name, chunks) in &asset_puts {
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&(chunks.len() as u32).to_le_bytes());
        for hex in chunks.iter() {
            out.extend_from_slice(&from_hex(hex).unwrap_or([0u8; 32]));
        }
        encode_record_ref(&mut out, index.records.get(*name));
    }
    out.extend_from_slice(&(asset_dels.len() as u32).to_le_bytes());
    for name in &asset_dels {
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
    }

    let body_len = (out.len() - JOURNAL_HEADER_SIZE) as u32;
    out[12..16].copy_from_slice(&body_len.to_le_bytes());
    let seal = cavs_hash::hash_chunk(&out);
    out.extend_from_slice(&seal);
    out
}

/// What a journal replay found: how much of the file held records the
/// ledger could take, and how long the file is. A difference is a tail no
/// replay can use.
struct JournalScan {
    truncate_to: u64,
    file_len: u64,
}

/// Apply every record of `path` that follows on from `index`'s generation.
/// A missing file is an empty journal. Never fails on the journal's own
/// contents — a torn or corrupt record ends the replay — only on I/O.
fn replay_journal(index: &mut Index, path: &Path) -> Result<JournalScan> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(JournalScan {
                truncate_to: 0,
                file_len: 0,
            })
        }
        Err(e) => return Err(e.into()),
    };
    let mut at = 0usize;
    while let Some((record, generation)) = next_journal_record(&bytes[at..]) {
        let len = record.len();
        if generation == index.generation + 1 {
            if apply_journal_record(index, &record[JOURNAL_HEADER_SIZE..len - JOURNAL_SEAL_SIZE])
                .is_err()
            {
                break;
            }
            index.generation = generation;
        } else if generation > index.generation + 1 {
            break; // a gap: nothing past it can apply
        }
        // else: older than the ledger — a rotated journal's record; skip.
        at += len;
    }
    Ok(JournalScan {
        truncate_to: at as u64,
        file_len: bytes.len() as u64,
    })
}

/// The sealed record at the start of `bytes` and its generation, or `None`
/// when what is there is not a whole, intact record.
fn next_journal_record(bytes: &[u8]) -> Option<(&[u8], u64)> {
    if bytes.len() < JOURNAL_HEADER_SIZE + JOURNAL_SEAL_SIZE || &bytes[..8] != JOURNAL_MAGIC {
        return None;
    }
    let version = u16::from_le_bytes(bytes[8..10].try_into().unwrap());
    if version > JOURNAL_VERSION {
        return None;
    }
    let body_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let total = JOURNAL_HEADER_SIZE + body_len + JOURNAL_SEAL_SIZE;
    if bytes.len() < total {
        return None;
    }
    let (sealed, seal) = bytes[..total].split_at(total - JOURNAL_SEAL_SIZE);
    if cavs_hash::hash_chunk(sealed) != <[u8; 32]>::try_from(seal).unwrap() {
        return None;
    }
    let generation = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    Some((&bytes[..total], generation))
}

fn apply_journal_record(index: &mut Index, body: &[u8]) -> Result<()> {
    let corrupt = |what: &str| StoreError::IndexCorrupt(what.to_string());
    let mut cur = Cursor { body, at: 0 };
    let pack_count = cur.u16()? as usize;
    let mut packs = Vec::with_capacity(pack_count.min(cur.remaining() / 2));
    for _ in 0..pack_count {
        let len = cur.u16()? as usize;
        let s = std::str::from_utf8(cur.take(len)?).map_err(|_| corrupt("pack id not utf-8"))?;
        packs.push(s.to_string());
    }
    let upserts = cur.u32()? as usize;
    if upserts > cur.remaining() / 70 {
        return Err(corrupt("upsert count exceeds record"));
    }
    for _ in 0..upserts {
        let hash: [u8; 32] = cur.take(32)?.try_into().unwrap();
        let len_raw = cur.u32()?;
        let len_stored = cur.u32()?;
        let flags = cur.u32()?;
        let refcount = cur.u64()?;
        let zero_since = match cur.u64()? {
            u64::MAX => None,
            v => Some(v),
        };
        let ord = cur.u16()?;
        let pack_offset = cur.u64()?;
        let pack = if ord == u16::MAX {
            None
        } else {
            Some(
                packs
                    .get(ord as usize)
                    .ok_or_else(|| corrupt("pack ordinal out of range"))?
                    .clone(),
            )
        };
        index.chunks.insert(
            to_hex(&hash),
            ChunkInfo {
                len_raw,
                len_stored,
                flags,
                refcount,
                zero_since,
                pack_offset: pack.is_some().then_some(pack_offset),
                pack,
            },
        );
    }
    let removals = cur.u32()? as usize;
    if removals > cur.remaining() / 32 {
        return Err(corrupt("removal count exceeds record"));
    }
    for _ in 0..removals {
        let hash: [u8; 32] = cur.take(32)?.try_into().unwrap();
        index.chunks.remove(&to_hex(&hash));
    }
    let asset_puts = cur.u32()? as usize;
    for _ in 0..asset_puts {
        let len = cur.u16()? as usize;
        let name = std::str::from_utf8(cur.take(len)?)
            .map_err(|_| corrupt("asset name not utf-8"))?
            .to_string();
        let n = cur.u32()? as usize;
        if n > cur.remaining() / 32 {
            return Err(corrupt("asset chunk count exceeds record"));
        }
        let mut hexes = Vec::with_capacity(n);
        for _ in 0..n {
            let hash: [u8; 32] = cur.take(32)?.try_into().unwrap();
            hexes.push(to_hex(&hash));
        }
        match decode_record_ref(&mut cur)? {
            Some(at) => index.records.insert(name.clone(), at),
            None => index.records.remove(&name),
        };
        index.assets.insert(name, hexes);
    }
    let asset_dels = cur.u32()? as usize;
    for _ in 0..asset_dels {
        let len = cur.u16()? as usize;
        let name =
            std::str::from_utf8(cur.take(len)?).map_err(|_| corrupt("asset name not utf-8"))?;
        index.assets.remove(name);
        index.records.remove(name);
    }
    if cur.remaining() != 0 {
        return Err(corrupt("trailing bytes"));
    }
    Ok(())
}

/// A bounds-checked reader over a sealed body; every read past the end is
/// the same `IndexCorrupt("truncated")`.
struct Cursor<'a> {
    body: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let s = self
            .body
            .get(self.at..self.at.saturating_add(n))
            .ok_or_else(|| StoreError::IndexCorrupt("truncated".into()))?;
        self.at += n;
        Ok(s)
    }
    fn remaining(&self) -> usize {
        self.body.len() - self.at
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

// --- binary ledger snapshot (index.bin) -----------------------------------
//
// Compact fixed-record format so a large store's open/save cost scales with
// chunk count, not JSON text size (the ledger is the one store structure
// that grows with every unique chunk). Layout, little-endian throughout:
//
//   header (self-describing, INDEX_HEADER_SIZE bytes):
//     "CAVSIDX1"        magic
//     u16 version       readers reject versions above their own
//     u16 header_size   body starts here (lets v1 grow header fields)
//     u16 record_size   size of one chunk record (validated before parse)
//     u16 flags         reserved, 0
//     u8  layout        0 = loose, 1 = packfiles
//     u8  reserved
//     u64 generation    monotonic save counter
//     u64 created_at    unix seconds of this save
//     6B  reserved
//   body:
//     u32 pack_count    { u16 len, hex bytes } × pack_count
//     u64 chunk_count   { hash 32B, len_raw u32, len_stored u32, flags u32,
//                         refcount u64, zero_since u64 (MAX = none),
//                         pack_ord u32 (MAX = none), pack_offset u64
//                       } × chunk_count, sorted by hex (BTreeMap order)
//     u32 asset_count   { u16 len, name bytes, u32 n, hash 32B × n,
//                         v2: u8 packed, if 1: pack 32B, u32 offset, u32 len
//                       } × count
//   BLAKE3 of everything above (32B seal)
//
// v2 (1.8) added the record location per asset. It is also the version
// from which a ledger may be followed by a journal (index.log): a v1
// reader rejects v2 rather than reading the snapshot and missing the saves
// the journal holds, which is what the version field is for.

const INDEX_MAGIC: &[u8; 8] = b"CAVSIDX1";
const INDEX_VERSION: u16 = 2;
const INDEX_HEADER_SIZE: u16 = 40;
const INDEX_RECORD_SIZE: u16 = 72;

fn encode_index(index: &Index) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + index.chunks.len() * INDEX_RECORD_SIZE as usize);
    out.extend_from_slice(INDEX_MAGIC);
    out.extend_from_slice(&INDEX_VERSION.to_le_bytes());
    out.extend_from_slice(&INDEX_HEADER_SIZE.to_le_bytes());
    out.extend_from_slice(&INDEX_RECORD_SIZE.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // flags
    out.push(match index.layout {
        StoreLayout::Loose => 0,
        StoreLayout::Packfiles => 1,
    });
    out.push(0);
    out.extend_from_slice(&index.generation.to_le_bytes());
    out.extend_from_slice(&now_epoch().to_le_bytes());
    out.extend_from_slice(&[0u8; 6]);
    debug_assert_eq!(out.len(), INDEX_HEADER_SIZE as usize);

    // Pack table: dedup pack ids so chunk records store a u32 ordinal.
    let mut packs: Vec<&str> = Vec::new();
    let mut pack_ord: HashMap<&str, u32> = HashMap::new();
    for info in index.chunks.values() {
        if let Some(p) = info.pack.as_deref() {
            if !pack_ord.contains_key(p) {
                pack_ord.insert(p, packs.len() as u32);
                packs.push(p);
            }
        }
    }
    out.extend_from_slice(&(packs.len() as u32).to_le_bytes());
    for p in &packs {
        out.extend_from_slice(&(p.len() as u16).to_le_bytes());
        out.extend_from_slice(p.as_bytes());
    }

    out.extend_from_slice(&(index.chunks.len() as u64).to_le_bytes());
    for (hex, info) in &index.chunks {
        // Ledger keys are always hex of 32B BLAKE3 (from_hex only fails on
        // a hand-corrupted store, encoded here as a zero hash — decode then
        // fails verification instead of silently dropping the entry).
        let hash = from_hex(hex).unwrap_or([0u8; 32]);
        out.extend_from_slice(&hash);
        out.extend_from_slice(&info.len_raw.to_le_bytes());
        out.extend_from_slice(&info.len_stored.to_le_bytes());
        out.extend_from_slice(&info.flags.to_le_bytes());
        out.extend_from_slice(&info.refcount.to_le_bytes());
        out.extend_from_slice(&info.zero_since.unwrap_or(u64::MAX).to_le_bytes());
        let ord = info
            .pack
            .as_deref()
            .and_then(|p| pack_ord.get(p).copied())
            .unwrap_or(u32::MAX);
        out.extend_from_slice(&ord.to_le_bytes());
        out.extend_from_slice(&info.pack_offset.unwrap_or(0).to_le_bytes());
    }

    out.extend_from_slice(&(index.assets.len() as u32).to_le_bytes());
    for (name, chunks) in &index.assets {
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&(chunks.len() as u32).to_le_bytes());
        for hex in chunks {
            out.extend_from_slice(&from_hex(hex).unwrap_or([0u8; 32]));
        }
        encode_record_ref(&mut out, index.records.get(name));
    }

    let seal = cavs_hash::hash_chunk(&out);
    out.extend_from_slice(&seal);
    out
}

fn encode_record_ref(out: &mut Vec<u8>, at: Option<&RecordRef>) {
    match at {
        Some(at) => {
            out.push(1);
            out.extend_from_slice(&at.pack);
            out.extend_from_slice(&at.offset.to_le_bytes());
            out.extend_from_slice(&at.len.to_le_bytes());
        }
        None => out.push(0),
    }
}

fn decode_record_ref(cur: &mut Cursor<'_>) -> Result<Option<RecordRef>> {
    match cur.take(1)?[0] {
        0 => Ok(None),
        1 => {
            let pack: [u8; 32] = cur.take(32)?.try_into().unwrap();
            let offset = cur.u32()?;
            let len = cur.u32()?;
            Ok(Some(RecordRef { pack, offset, len }))
        }
        _ => Err(StoreError::IndexCorrupt("bad record location tag".into())),
    }
}

/// Whether `bytes` is a whole snapshot whose BLAKE3 seal matches its body.
/// What a decode would check first, at the cost of a hash rather than of
/// materializing the ledger.
fn index_seal_holds(bytes: &[u8]) -> bool {
    if bytes.len() < INDEX_HEADER_SIZE as usize + 32 {
        return false;
    }
    let (body, seal) = bytes.split_at(bytes.len() - 32);
    cavs_hash::hash_chunk(body) == <[u8; 32]>::try_from(seal).unwrap()
}

fn decode_index(bytes: &[u8]) -> Result<Index> {
    decode_index_versioned(bytes).map(|(index, _)| index)
}

/// Decode a snapshot and say which format version wrote it.
fn decode_index_versioned(bytes: &[u8]) -> Result<(Index, u16)> {
    let corrupt = |what: &str| StoreError::IndexCorrupt(what.to_string());
    if bytes.len() < INDEX_HEADER_SIZE as usize + 32 {
        return Err(corrupt("truncated"));
    }
    if !index_seal_holds(bytes) {
        return Err(corrupt("seal mismatch"));
    }
    let body = &bytes[..bytes.len() - 32];
    if &body[..8] != INDEX_MAGIC {
        return Err(corrupt("bad magic"));
    }
    let mut cur = Cursor { body, at: 8 };
    macro_rules! take {
        ($n:expr) => {
            cur.take($n)
        };
    }
    let u16le = |s: &[u8]| u16::from_le_bytes(s.try_into().unwrap());
    let u32le = |s: &[u8]| u32::from_le_bytes(s.try_into().unwrap());
    let u64le = |s: &[u8]| u64::from_le_bytes(s.try_into().unwrap());

    let version = u16le(take!(2)?);
    if version > INDEX_VERSION {
        return Err(corrupt(&format!(
            "index version {version} was written by a newer CAVS; this build reads up to {INDEX_VERSION}"
        )));
    }
    let header_size = u16le(take!(2)?) as usize;
    if header_size < INDEX_HEADER_SIZE as usize || header_size >= body.len() {
        return Err(corrupt("bad header size"));
    }
    let record_size = u16le(take!(2)?);
    if record_size != INDEX_RECORD_SIZE {
        return Err(corrupt(&format!(
            "record size {record_size} unsupported (expected {INDEX_RECORD_SIZE})"
        )));
    }
    take!(2)?; // flags
    let layout = match take!(1)?[0] {
        0 => StoreLayout::Loose,
        1 => StoreLayout::Packfiles,
        _ => return Err(corrupt("bad layout")),
    };
    take!(1)?; // reserved
    let generation = u64le(take!(8)?);
    take!(8)?; // created_at
    take!(header_size - 34)?; // 34 bytes read so far; skip any v1.x header growth

    let pack_count = u32le(take!(4)?) as usize;
    // Counts come from untrusted bytes: never let a crafted count reserve
    // more memory than the file could possibly describe (2B minimum/pack).
    if pack_count > body.len() / 2 {
        return Err(corrupt("pack count exceeds file size"));
    }
    let mut packs = Vec::with_capacity(pack_count);
    for _ in 0..pack_count {
        let len = u16le(take!(2)?) as usize;
        let s = std::str::from_utf8(take!(len)?).map_err(|_| corrupt("pack id not utf-8"))?;
        packs.push(s.to_string());
    }

    let chunk_count = u64le(take!(8)?) as usize;
    if chunk_count
        .checked_mul(INDEX_RECORD_SIZE as usize)
        .is_none_or(|need| need > cur.remaining())
    {
        return Err(corrupt("chunk count exceeds file size"));
    }
    let mut chunks = BTreeMap::new();
    for _ in 0..chunk_count {
        let hash: [u8; 32] = take!(32)?.try_into().unwrap();
        let len_raw = u32le(take!(4)?);
        let len_stored = u32le(take!(4)?);
        let flags = u32le(take!(4)?);
        let refcount = u64le(take!(8)?);
        let zero_since = match u64le(take!(8)?) {
            u64::MAX => None,
            v => Some(v),
        };
        let ord = u32le(take!(4)?);
        let pack_offset = u64le(take!(8)?);
        let pack = if ord == u32::MAX {
            None
        } else {
            Some(
                packs
                    .get(ord as usize)
                    .ok_or_else(|| corrupt("pack ordinal out of range"))?
                    .clone(),
            )
        };
        chunks.insert(
            to_hex(&hash),
            ChunkInfo {
                len_raw,
                len_stored,
                flags,
                refcount,
                zero_since,
                pack_offset: pack.is_some().then_some(pack_offset),
                pack,
            },
        );
    }

    let asset_count = u32le(take!(4)?) as usize;
    let mut assets = BTreeMap::new();
    let mut records = BTreeMap::new();
    for _ in 0..asset_count {
        let len = u16le(take!(2)?) as usize;
        let name = std::str::from_utf8(take!(len)?)
            .map_err(|_| corrupt("asset name not utf-8"))?
            .to_string();
        let n = u32le(take!(4)?) as usize;
        if n > cur.remaining() / 32 {
            return Err(corrupt("asset chunk count exceeds file size"));
        }
        let mut hexes = Vec::with_capacity(n);
        for _ in 0..n {
            let hash: [u8; 32] = take!(32)?.try_into().unwrap();
            hexes.push(to_hex(&hash));
        }
        if version >= 2 {
            if let Some(at) = decode_record_ref(&mut cur)? {
                records.insert(name.clone(), at);
            }
        }
        assets.insert(name, hexes);
    }
    if cur.remaining() != 0 {
        return Err(corrupt("trailing bytes"));
    }
    Ok((
        Index {
            chunks,
            assets,
            records,
            layout,
            generation,
        },
        version,
    ))
}

/// Copy `src` to `dst` unless `dst` already exists with the same length.
/// Packs and their indexes are immutable and content-addressed, so an
/// equal-length destination is the same object — skipping the copy makes
/// re-exports into the same tree effectively incremental. Returns whether
/// a copy happened.
/// Inverse of the BG4 byte-grouping pretransform (mirrors
/// `cavs_format::bg4_ungroup`; duplicated to avoid a dependency cycle —
/// cavs-format depends on this crate).
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

fn copy_if_different(src: &Path, dst: &Path) -> Result<bool> {
    std::fs::create_dir_all(dst.parent().unwrap())?;
    let same = match (std::fs::metadata(src), std::fs::metadata(dst)) {
        (Ok(s), Ok(d)) => s.len() == d.len(),
        _ => false,
    };
    if !same {
        std::fs::copy(src, dst)?;
    }
    Ok(!same)
}

/// Sorted merge of the segmented index's live view with the store's
/// uncommitted overlay: overlay entries shadow the base (and `None`
/// entries delete), preserving hex order so callers see one coherent,
/// sorted ledger stream.
struct OverlayMerge<'a, B: Iterator<Item = (String, ChunkInfo)>> {
    base: std::iter::Peekable<B>,
    overlay: std::iter::Peekable<std::collections::btree_map::Iter<'a, String, Option<ChunkInfo>>>,
}

impl<B: Iterator<Item = (String, ChunkInfo)>> Iterator for OverlayMerge<'_, B> {
    type Item = (String, ChunkInfo);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let which = match (self.base.peek(), self.overlay.peek()) {
                (None, None) => return None,
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (Some((bh, _)), Some((oh, _))) => bh.as_str().cmp(oh.as_str()),
            };
            match which {
                std::cmp::Ordering::Less => return self.base.next(),
                std::cmp::Ordering::Greater => {
                    let (hex, entry) = self.overlay.next().unwrap();
                    if let Some(info) = entry {
                        return Some((hex.clone(), info.clone()));
                    }
                    // Overlay-only tombstone (entry deleted twice): skip.
                }
                std::cmp::Ordering::Equal => {
                    self.base.next(); // shadowed
                    let (hex, entry) = self.overlay.next().unwrap();
                    if let Some(info) = entry {
                        return Some((hex.clone(), info.clone()));
                    }
                    // Tombstone: the base entry is deleted; keep merging.
                }
            }
        }
    }
}

/// One `meta/index.json` entry: a session meta-pack and the oids it holds.
struct MetaIndexEntry {
    id: String,
    oids: Vec<String>,
}

/// Read the meta index, or rebuild it by scanning `meta/packs/*.cmeta` when
/// it is missing or unreadable (the packs are the source of truth; the
/// index is a derived accelerator, so corruption self-heals).
fn read_or_rebuild_meta_index(index_path: &Path, packs_dir: &Path) -> Vec<MetaIndexEntry> {
    if let Ok(bytes) = std::fs::read(index_path) {
        if let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            if doc.get("version").and_then(|v| v.as_u64()) == Some(1) {
                if let Some(packs) = doc.get("packs").and_then(|p| p.as_array()) {
                    let mut out = Vec::with_capacity(packs.len());
                    for p in packs {
                        let (Some(id), Some(oids)) = (
                            p.get("id").and_then(|v| v.as_str()),
                            p.get("oids").and_then(|v| v.as_array()),
                        ) else {
                            continue;
                        };
                        out.push(MetaIndexEntry {
                            id: id.to_string(),
                            oids: oids
                                .iter()
                                .filter_map(|o| o.as_str().map(str::to_string))
                                .collect(),
                        });
                    }
                    return out;
                }
            }
        }
    }
    // Rebuild from the packs themselves. Sort by mtime so "later pack wins"
    // still resolves re-pushed oids to their newest metadata.
    let mut found: Vec<(std::time::SystemTime, MetaIndexEntry)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(packs_dir) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("cmeta") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(compressed) = std::fs::read(&path) else {
            continue;
        };
        let Ok(raw) = zstd::bulk::decompress(&compressed, 256 * 1024 * 1024) else {
            continue;
        };
        let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&raw) else {
            continue;
        };
        let Some(objects) = doc.get("objects").and_then(|o| o.as_array()) else {
            continue;
        };
        let oids: Vec<String> = objects
            .iter()
            .filter_map(|o| o.get("oid").and_then(|v| v.as_str()).map(str::to_string))
            .collect();
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        found.push((
            mtime,
            MetaIndexEntry {
                id: id.to_string(),
                oids,
            },
        ));
    }
    found.sort_by_key(|(mtime, _)| *mtime);
    found.into_iter().map(|(_, e)| e).collect()
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cavs_hash::hash_chunk;

    fn rec(name: &str, chunks: &[&ChunkHash]) -> AssetRecord {
        AssetRecord {
            name: name.into(),
            asset_uuid: "0".repeat(32),
            tracks: vec![],
            segments: vec![StoreSegment {
                segment_id: 0,
                track_id: 0,
                pts_start: 0,
                duration: 0,
                random_access: true,
                chunks: chunks.iter().map(|h| to_hex(h)).collect(),
            }],
            dict: vec![],
            chunk_table: chunks.iter().map(|h| to_hex(h)).collect(),
            merkle_root: String::new(),
            signature: None,
            signer_pubkey: None,
            meta: vec![],
        }
    }

    #[test]
    fn publish_batch_is_atomic_and_aggregates_packs() {
        let dir = tempfile::tempdir().unwrap();
        let a = vec![1u8; 1000];
        let b = vec![2u8; 1000];
        let (ha, hb) = (hash_chunk(&a), hash_chunk(&b));

        {
            let mut store =
                GlobalStore::open_with_layout(dir.path(), Some(StoreLayout::Packfiles)).unwrap();
            let ledger_at_creation = std::fs::read(dir.path().join("index.bin")).unwrap();
            store.begin_publish_batch();
            assert!(store.put_chunk(&ha, &a, 0, a.len() as u32).unwrap());
            store.publish_asset(&rec("v1", &[&ha])).unwrap();
            assert!(store.put_chunk(&hb, &b, 0, b.len() as u32).unwrap());
            store.publish_asset(&rec("v2", &[&ha, &hb])).unwrap();

            // In-memory ledger sees both; nothing is on disk yet (a crash
            // here must leave the store exactly as before the batch). Disk
            // state is checked directly — opening a second store would sweep
            // the batch's open .part pack (writers are lock-serialized in
            // real use).
            assert!(store.has_asset("v1") && store.has_asset("v2"));
            assert!(store.get_asset("v1").is_err(), "record file deferred");
            assert_eq!(
                std::fs::read(dir.path().join("index.bin")).unwrap(),
                ledger_at_creation,
                "ledger deferred"
            );
            assert!(!dir.path().join("assets/v1.json").exists());

            store.commit_publish_batch().unwrap();
        }

        // Reopen: everything from the batch is persisted, and both assets
        // share ONE aggregated pack (not one pack per publish).
        let store = GlobalStore::open(dir.path()).unwrap();
        assert!(store.get_asset("v1").is_ok() && store.get_asset("v2").is_ok());
        assert_eq!(store.chunk_info(&ha).unwrap().refcount, 2);
        assert_eq!(store.chunk_info(&hb).unwrap().refcount, 1);
        let stats = store.stats();
        assert_eq!(stats.unique_chunks, 2);
        assert_eq!(stats.pack_count, 1, "batch aggregates into one pack");
        assert_eq!(store.verify().unwrap(), 2);
    }

    #[test]
    fn segmented_store_full_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let a = vec![1u8; 1500];
        let b = vec![2u8; 1500];
        let c = vec![3u8; 1500];
        let (ha, hb, hc) = (hash_chunk(&a), hash_chunk(&b), hash_chunk(&c));

        // Populate a legacy store, then migrate.
        {
            let mut store =
                GlobalStore::open_with_layout(dir.path(), Some(StoreLayout::Packfiles)).unwrap();
            store.put_chunk(&ha, &a, 0, a.len() as u32).unwrap();
            store.put_chunk(&hb, &b, 0, b.len() as u32).unwrap();
            store.publish_asset(&rec("v1", &[&ha, &hb])).unwrap();
            assert_eq!(store.migrate_index_to_segmented().unwrap(), 2);
            assert!(store.is_segmented());
            assert!(!dir.path().join("index.bin").exists());
            assert!(dir.path().join("index.bin.pre-migration").exists());
            // Reads work through the mmapped segments.
            assert_eq!(store.chunk_info(&ha).unwrap().refcount, 1);
            assert_eq!(store.verify().unwrap(), 2);
        }

        // Reopen (goes straight to the segmented path) and keep working:
        // publish a new asset with a new chunk, replace, GC.
        {
            let mut store = GlobalStore::open(dir.path()).unwrap();
            assert!(store.is_segmented());
            assert!(store.has_asset("v1"));
            store.put_chunk(&hc, &c, 0, c.len() as u32).unwrap();
            store.publish_asset(&rec("v2", &[&hb, &hc])).unwrap();
            assert_eq!(store.chunk_info(&hb).unwrap().refcount, 2);

            // Replace v1 by v2-only content: ha drops to zero.
            store.unpublish_asset("v1").unwrap();
            assert_eq!(store.chunk_info(&ha).unwrap().refcount, 0);
            let (removed, _) = store.gc(0).unwrap();
            assert_eq!(removed, 1);
            assert!(store.chunk_info(&ha).is_none(), "gc'd through tombstone");
        }

        // Final reopen: the tombstone survived the generation swap.
        let store = GlobalStore::open(dir.path()).unwrap();
        assert!(store.chunk_info(&ha).is_none());
        assert_eq!(store.chunk_info(&hc).unwrap().refcount, 1);
        assert_eq!(store.stats().unique_chunks, 2);
        assert_eq!(store.verify().unwrap(), 2);
    }

    #[test]
    fn segmented_store_batch_publish_and_export() {
        let dir = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let mut store =
            GlobalStore::open_with_layout(dir.path(), Some(StoreLayout::Packfiles)).unwrap();
        store.migrate_index_to_segmented().unwrap();

        let a = vec![7u8; 4000];
        let b = vec![8u8; 4000];
        let (ha, hb) = (hash_chunk(&a), hash_chunk(&b));
        store.begin_publish_batch();
        store.put_chunk(&ha, &a, 0, a.len() as u32).unwrap();
        store.publish_asset(&rec("o1", &[&ha])).unwrap();
        store.put_chunk(&hb, &b, 0, b.len() as u32).unwrap();
        store.publish_asset(&rec("o2", &[&ha, &hb])).unwrap();
        store.commit_publish_batch().unwrap();

        store.export_asset("o1", tree.path()).unwrap();
        store.export_asset("o2", tree.path()).unwrap();
        store
            .export_meta_pack(&["o1".into(), "o2".into()], tree.path())
            .unwrap()
            .unwrap();
        assert!(tree.path().join("assets/o1/manifest.json").is_file());
        assert!(tree.path().join("assets/o2/chunk-map.json").is_file());
        assert!(tree.path().join("meta/index.json").is_file());

        // Reopen: everything persisted via delta segments.
        let store = GlobalStore::open(dir.path()).unwrap();
        assert_eq!(store.stats().assets, 2);
        assert_eq!(store.chunk_info(&ha).unwrap().refcount, 2);
    }

    /// Scale probe (ignored in CI): 1M chunks through migration, lookups
    /// and a delta commit. Run with
    /// `cargo test -p cavs-store --release -- --ignored index_scale_segmented`.
    #[test]
    #[ignore]
    fn index_scale_segmented_1m_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let n: usize = 1_000_000;
        let mut chunks: BTreeMap<String, ChunkInfo> = BTreeMap::new();
        for i in 0..n {
            let h = to_hex(&hash_chunk(&(i as u64).to_le_bytes()));
            chunks.insert(
                h,
                ChunkInfo {
                    len_raw: 16 * 1024,
                    len_stored: 8 * 1024,
                    flags: 1,
                    refcount: 1,
                    zero_since: None,
                    pack: Some(format!("{:064x}", i / 4096)),
                    pack_offset: Some(((i % 4096) * 8192) as u64),
                },
            );
        }
        let assets = BTreeMap::from([(
            "big".to_string(),
            chunks.keys().take(1000).cloned().collect::<Vec<_>>(),
        )]);
        let t = std::time::Instant::now();
        let (seg, _) = crate::segindex::SegIndex::create(
            dir.path(),
            1,
            StoreLayout::Packfiles,
            &chunks,
            &assets,
        )
        .unwrap();
        eprintln!("create 1M: {:?}", t.elapsed());
        drop(seg);

        let t = std::time::Instant::now();
        let (seg, _) = crate::segindex::SegIndex::open(dir.path()).unwrap();
        let open_elapsed = t.elapsed();
        eprintln!("open 1M: {open_elapsed:?}");
        assert!(open_elapsed.as_millis() < 1000, "open must be sub-second");

        let keys: Vec<&String> = chunks.keys().step_by(997).collect();
        let t = std::time::Instant::now();
        for k in &keys {
            assert!(seg.lookup(k).is_some());
        }
        eprintln!(
            "lookup p50 over {} probes: {:?}/probe",
            keys.len(),
            t.elapsed() / keys.len() as u32
        );
    }

    #[test]
    fn repack_merges_small_packs_copy_on_write() {
        let dir = tempfile::tempdir().unwrap();
        let mut store =
            GlobalStore::open_with_layout(dir.path(), Some(StoreLayout::Packfiles)).unwrap();
        // Force one tiny pack per publish: 20 small packs.
        store.set_preferred_pack_size(1);
        let mut hashes = Vec::new();
        for i in 0..20u8 {
            let data = vec![i; 3000];
            let h = hash_chunk(&data);
            store.put_chunk(&h, &data, 0, data.len() as u32).unwrap();
            store.publish_asset(&rec(&format!("a{i}"), &[&h])).unwrap();
            hashes.push(h);
        }
        let before = store.fragmentation();
        assert_eq!(before.pack_count, 20);
        assert_eq!(before.small_packs, 20);

        // Merge them with a sane target size again.
        store.set_preferred_pack_size(128 * 1024 * 1024);
        let plan = store.repack_plan();
        assert!(!plan.is_empty());
        let outcome = store.repack_run(&plan, false).unwrap();
        assert_eq!(outcome.packs_rewritten, 20);
        assert_eq!(outcome.chunks_moved, 20);
        assert_eq!(outcome.quarantined.len(), 20);

        let after = store.fragmentation();
        assert!(
            after.pack_count as f64 <= before.pack_count as f64 * 0.3,
            "pack count must drop >=70% (before {}, after {})",
            before.pack_count,
            after.pack_count
        );
        // Copy-on-write: every chunk still reads back and verifies.
        assert_eq!(store.verify().unwrap(), 20);
        for h in &hashes {
            assert!(store.read_chunk_stored(h).is_ok());
        }

        // Reopen: the repacked ledger persisted; integrity holds.
        drop(store);
        let store = GlobalStore::open(dir.path()).unwrap();
        assert_eq!(store.verify().unwrap(), 20);
    }

    #[test]
    fn repack_compacts_dead_bytes_and_is_dry_run_safe() {
        let dir = tempfile::tempdir().unwrap();
        let mut store =
            GlobalStore::open_with_layout(dir.path(), Some(StoreLayout::Packfiles)).unwrap();
        store.set_preferred_pack_size(1024 * 1024 * 1024);
        // One pack: 10 chunks, then unpublish+gc 4 of them (~40% dead).
        // Individual chunks stay under the small-pack threshold, so make
        // the pack big enough to be a *compaction* candidate.
        let mut live_hashes = Vec::new();
        let mut dead_recs = Vec::new();
        for i in 0..10u8 {
            let data = vec![i; 2 * 1024 * 1024];
            let h = hash_chunk(&data);
            store.put_chunk(&h, &data, 0, data.len() as u32).unwrap();
            if i < 4 {
                dead_recs.push((format!("dead{i}"), h));
            } else {
                live_hashes.push((format!("live{i}"), h));
            }
        }
        let all: Vec<&ChunkHash> = dead_recs
            .iter()
            .map(|(_, h)| h)
            .chain(live_hashes.iter().map(|(_, h)| h))
            .collect();
        store.publish_asset(&rec("everything", &all)).unwrap();
        for (name, h) in &live_hashes {
            store.publish_asset(&rec(name, &[h])).unwrap();
        }
        // Drop the umbrella asset: the 4 dead-only chunks hit refcount 0.
        store.unpublish_asset("everything").unwrap();
        store.gc(0).unwrap();

        let frag = store.fragmentation();
        assert_eq!(frag.pack_count, 1);
        assert!(
            frag.dead_bytes_ratio > 0.35,
            "expected ~40% dead, got {:.2}",
            frag.dead_bytes_ratio
        );

        // Dry run: reports work, changes nothing.
        let plan = store.repack_plan();
        assert_eq!(plan.compact_packs.len(), 1);
        let dry = store.repack_run(&plan, true).unwrap();
        assert_eq!(dry.chunks_moved, 6);
        assert_eq!(store.fragmentation().pack_count, 1, "dry run wrote nothing");
        assert!(dry.quarantined.is_empty());

        // Real run reclaims ~all dead bytes.
        let outcome = store.repack_run(&plan, false).unwrap();
        assert_eq!(outcome.chunks_moved, 6);
        let after = store.fragmentation();
        assert!(
            after.dead_bytes_ratio < 0.05,
            "dead bytes reclaimed, got {:.2}",
            after.dead_bytes_ratio
        );
        assert_eq!(store.verify().unwrap(), 6);
    }

    #[test]
    fn meta_pack_export_writes_pack_and_self_healing_index() {
        let dir = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let mut store =
            GlobalStore::open_with_layout(dir.path(), Some(StoreLayout::Packfiles)).unwrap();
        let a = vec![1u8; 1000];
        let b = vec![2u8; 1000];
        let (ha, hb) = (hash_chunk(&a), hash_chunk(&b));
        store.put_chunk(&ha, &a, 0, a.len() as u32).unwrap();
        store.put_chunk(&hb, &b, 0, b.len() as u32).unwrap();
        store.publish_asset(&rec("oid1", &[&ha])).unwrap();
        store.publish_asset(&rec("oid2", &[&ha, &hb])).unwrap();

        let id = store
            .export_meta_pack(&["oid1".into(), "oid2".into()], tree.path())
            .unwrap()
            .expect("a pack id");
        let pack_path = tree.path().join(format!("meta/packs/{id}.cmeta"));
        assert!(pack_path.is_file());

        // The pack carries both objects' manifests + chunk locations,
        // run-encoded (v2): oid2's two contiguous chunks form ONE run.
        let raw = zstd::bulk::decompress(&std::fs::read(&pack_path).unwrap(), 1 << 30).unwrap();
        let doc: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(doc["version"], 1);
        assert_eq!(doc["objects"].as_array().unwrap().len(), 2);
        let runs = doc["objects"][1]["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 1, "contiguous chunks collapse into one run");
        assert_eq!(runs[0]["hashes"].as_array().unwrap().len(), 2);
        assert_eq!(runs[0]["flags"], 0, "uniform flags collapse to a scalar");

        // Run encoding must be smaller than the per-chunk v1 encoding.
        let v1_bytes = serde_json::to_vec(&serde_json::json!({
            "objects": [{
                "oid": "oid2",
                "chunks": store.chunk_map_entries("oid2").unwrap(),
            }],
        }))
        .unwrap()
        .len();
        let v2_bytes = serde_json::to_vec(&serde_json::json!({
            "objects": [{
                "oid": "oid2",
                "runs": store.chunk_map_runs("oid2").unwrap(),
            }],
        }))
        .unwrap()
        .len();
        assert!(
            (v2_bytes as f64) < v1_bytes as f64 * 0.7,
            "runs must cut location metadata by >30% (v1 {v1_bytes} vs v2 {v2_bytes})"
        );

        // The index maps both oids to the pack.
        let index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(tree.path().join("meta/index.json")).unwrap())
                .unwrap();
        assert_eq!(index["packs"][0]["id"], id.as_str());
        assert_eq!(index["packs"][0]["oids"].as_array().unwrap().len(), 2);

        // A second session appends; a corrupted index self-heals from the
        // packs on the next export.
        store.publish_asset(&rec("oid3", &[&hb])).unwrap();
        std::fs::write(tree.path().join("meta/index.json"), b"garbage").unwrap();
        let id2 = store
            .export_meta_pack(&["oid3".into()], tree.path())
            .unwrap()
            .unwrap();
        let index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(tree.path().join("meta/index.json")).unwrap())
                .unwrap();
        let packs = index["packs"].as_array().unwrap();
        assert_eq!(packs.len(), 2, "rebuilt old pack + appended new one");
        assert!(packs.iter().any(|p| p["id"] == id.as_str()));
        assert!(packs.iter().any(|p| p["id"] == id2.as_str()));
    }

    #[test]
    fn gc_sweeps_orphan_packs() {
        let dir = tempfile::tempdir().unwrap();
        let mut store =
            GlobalStore::open_with_layout(dir.path(), Some(StoreLayout::Packfiles)).unwrap();
        let a = vec![7u8; 2000];
        let ha = hash_chunk(&a);
        store.put_chunk(&ha, &a, 0, a.len() as u32).unwrap();
        store.publish_asset(&rec("live", &[&ha])).unwrap();

        // A sealed pack no ledger entry references — what a session that
        // rolled over a pack but died before commit leaves behind.
        let orphan = dir
            .path()
            .join("packs/de/dead".to_owned() + &"be".repeat(30) + ".cavspack");
        std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
        std::fs::write(&orphan, b"orphaned pack bytes").unwrap();

        let (_removed, bytes) = store.gc(0).unwrap();
        assert!(!orphan.exists(), "orphan pack must be swept");
        assert!(bytes >= 19, "reclaimed bytes must count the orphan");
        // The referenced pack survives and the store still verifies.
        assert_eq!(store.verify().unwrap(), 1);
    }

    #[test]
    fn binary_index_roundtrip_and_corruption_detection() {
        let mut index = Index {
            layout: StoreLayout::Packfiles,
            ..Index::default()
        };
        let pack = "ab".to_string() + &"cd".repeat(31);
        for i in 0u64..500 {
            let h = hash_chunk(&i.to_le_bytes());
            index.chunks.insert(
                to_hex(&h),
                ChunkInfo {
                    len_raw: 1000 + i as u32,
                    len_stored: 900,
                    flags: (i % 4) as u32,
                    refcount: i % 3,
                    zero_since: (i % 3 == 0).then_some(i),
                    pack: (i % 2 == 0).then(|| pack.clone()),
                    pack_offset: (i % 2 == 0).then_some(i * 900),
                },
            );
        }
        index.assets.insert(
            "app".into(),
            index.chunks.keys().take(40).cloned().collect(),
        );
        index.assets.insert("flat".into(), vec![]);
        index.records.insert(
            "app".into(),
            RecordRef {
                pack: [7u8; 32],
                offset: 1234,
                len: 567,
            },
        );

        let bytes = encode_index(&index);
        let back = decode_index(&bytes).unwrap();
        assert_eq!(back.layout, index.layout);
        assert_eq!(back.assets, index.assets);
        assert_eq!(back.records, index.records);
        assert_eq!(back.chunks.len(), index.chunks.len());
        for (hex, info) in &index.chunks {
            let b = &back.chunks[hex];
            assert_eq!(
                (b.len_raw, b.len_stored, b.flags, b.refcount, b.zero_since),
                (
                    info.len_raw,
                    info.len_stored,
                    info.flags,
                    info.refcount,
                    info.zero_since
                )
            );
            assert_eq!((&b.pack, b.pack_offset), (&info.pack, info.pack_offset));
        }

        // Any bit flip must be caught by the BLAKE3 seal.
        let mut corrupt = bytes.clone();
        corrupt[100] ^= 1;
        assert!(matches!(
            decode_index(&corrupt),
            Err(StoreError::IndexCorrupt(_))
        ));
        assert!(matches!(
            decode_index(&bytes[..40]),
            Err(StoreError::IndexCorrupt(_))
        ));
    }

    #[test]
    fn legacy_json_index_is_read_and_migrated_on_save() {
        let dir = tempfile::tempdir().unwrap();
        let a = vec![5u8; 800];
        let ha = hash_chunk(&a);
        // A pre-1.6 store: index.json on disk, no index.bin.
        {
            let mut store = GlobalStore::open(dir.path()).unwrap();
            store.put_chunk(&ha, &a, 0, a.len() as u32).unwrap();
            store.publish_asset(&rec("old", &[&ha])).unwrap();
            let json = serde_json::to_vec_pretty(&store.index).unwrap();
            std::fs::write(dir.path().join("index.json"), json).unwrap();
            std::fs::remove_file(dir.path().join("index.bin")).unwrap();
            // A pre-1.6 store has no binary snapshots at all.
            let _ = std::fs::remove_file(dir.path().join("index.bin.prev"));
        }
        // Opens from index.json; the next save migrates to index.bin.
        let mut store = GlobalStore::open(dir.path()).unwrap();
        assert!(store.has_asset("old"));
        store.save_index().unwrap();
        assert!(dir.path().join("index.bin").exists());
        assert!(!dir.path().join("index.json").exists());
        assert!(GlobalStore::open(dir.path()).unwrap().has_asset("old"));
    }

    #[test]
    fn corrupt_index_falls_back_to_previous_generation() {
        let dir = tempfile::tempdir().unwrap();
        let a = vec![1u8; 500];
        let b = vec![2u8; 500];
        let (ha, hb) = (hash_chunk(&a), hash_chunk(&b));
        {
            let mut store = GlobalStore::open(dir.path()).unwrap();
            // Every save a snapshot: this is the snapshot pair's recovery.
            store.set_journal_budget(0);
            store.put_chunk(&ha, &a, 0, a.len() as u32).unwrap();
            store.publish_asset(&rec("first", &[&ha])).unwrap();
            store.put_chunk(&hb, &b, 0, b.len() as u32).unwrap();
            store.publish_asset(&rec("second", &[&hb])).unwrap();
        }
        let bin = dir.path().join("index.bin");
        let prev = dir.path().join("index.bin.prev");
        assert!(prev.exists(), "save keeps one previous generation");

        // Corrupt the live snapshot: open recovers from the previous
        // generation (one publish behind) instead of failing.
        let mut bytes = std::fs::read(&bin).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        std::fs::write(&bin, &bytes).unwrap();
        let store = GlobalStore::open(dir.path()).unwrap();
        assert!(store.has_asset("first"));
        assert!(!store.has_asset("second"), "prev is one generation behind");

        // A crash between save's two renames leaves only .prev: same story.
        std::fs::remove_file(&bin).unwrap();
        assert!(GlobalStore::open(dir.path()).unwrap().has_asset("first"));

        // Both generations corrupt: a clear error, never a silent new store.
        std::fs::write(&bin, b"garbage").unwrap();
        std::fs::write(&prev, b"garbage").unwrap();
        let _ = std::fs::remove_file(dir.path().join("index.json"));
        assert!(matches!(
            GlobalStore::open(dir.path()),
            Err(StoreError::IndexCorrupt(_))
        ));
    }

    /// Everything a store can say about one chunk, for equality across a
    /// reopen.
    fn chunk_view(store: &GlobalStore, hash: &ChunkHash) -> Option<ChunkInfo> {
        store.chunk_info(hash)
    }

    #[test]
    fn saves_append_to_the_journal_and_replay_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let bodies: Vec<Vec<u8>> = (0u8..6).map(|i| vec![i; 400 + i as usize]).collect();
        let hashes: Vec<ChunkHash> = bodies.iter().map(|b| hash_chunk(b)).collect();
        let snapshot_before;
        {
            let mut store =
                GlobalStore::open_with_layout(dir.path(), Some(StoreLayout::Packfiles)).unwrap();
            snapshot_before = std::fs::read(dir.path().join("index.bin")).unwrap();
            for (i, (body, hash)) in bodies.iter().zip(&hashes).enumerate() {
                store.put_chunk(hash, body, 0, body.len() as u32).unwrap();
                // Two assets share chunk 0: refcounts and a replace are
                // journaled too.
                store
                    .publish_asset(&rec(&format!("a{i}"), &[hash, &hashes[0]]))
                    .unwrap();
            }
            store.publish_asset(&rec("a1", &[&hashes[1]])).unwrap(); // replace
            assert!(store.unpublish_asset("a2").unwrap());
            let report = store.index_report();
            assert!(report.journal_bytes > 0, "saves went to the journal");
            assert_eq!(report.generation, 8);
        }
        // The snapshot was never rewritten; the journal carries every save.
        assert_eq!(
            std::fs::read(dir.path().join("index.bin")).unwrap(),
            snapshot_before
        );
        assert!(dir.path().join("index.log").exists());

        let store = GlobalStore::open(dir.path()).unwrap();
        assert_eq!(store.index_report().generation, 8);
        for i in [0usize, 1, 3, 4, 5] {
            assert!(store.has_asset(&format!("a{i}")), "a{i}");
        }
        assert!(!store.has_asset("a2"));
        // Chunk 0 is referenced by a0, a3, a4, a5 (a1 dropped it, a2 is gone).
        let c0 = chunk_view(&store, &hashes[0]).unwrap();
        assert_eq!(c0.refcount, 4);
        assert!(
            c0.pack.is_some() && c0.pack_offset.is_some(),
            "location replayed"
        );
        let c2 = chunk_view(&store, &hashes[2]).unwrap();
        assert_eq!(c2.refcount, 0);
        assert!(c2.zero_since.is_some());
        assert_eq!(store.get_asset("a1").unwrap().chunk_table.len(), 1);
        assert_eq!(store.read_chunk_stored(&hashes[4]).unwrap().0, bodies[4]);
    }

    #[test]
    fn a_torn_journal_tail_is_dropped_and_the_next_save_lands_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let bodies: Vec<Vec<u8>> = (10u8..14).map(|i| vec![i; 300]).collect();
        let hashes: Vec<ChunkHash> = bodies.iter().map(|b| hash_chunk(b)).collect();
        {
            let mut store = GlobalStore::open(dir.path()).unwrap();
            for (i, (body, hash)) in bodies.iter().zip(&hashes).enumerate().take(3) {
                store.put_chunk(hash, body, 0, body.len() as u32).unwrap();
                store
                    .publish_asset(&rec(&format!("t{i}"), &[hash]))
                    .unwrap();
            }
        }
        let log = dir.path().join("index.log");
        let intact = std::fs::read(&log).unwrap();

        // A crash mid-append leaves half a record: replay keeps the three
        // whole ones and the file is cut back to them.
        let mut torn = intact.clone();
        torn.extend_from_slice(&intact[..JOURNAL_HEADER_SIZE + 10]);
        std::fs::write(&log, &torn).unwrap();
        {
            let mut store = GlobalStore::open(dir.path()).unwrap();
            assert!(store.has_asset("t0") && store.has_asset("t1") && store.has_asset("t2"));
            assert_eq!(std::fs::metadata(&log).unwrap().len(), intact.len() as u64);
            store.put_chunk(&hashes[3], &bodies[3], 0, 300).unwrap();
            store.publish_asset(&rec("t3", &[&hashes[3]])).unwrap();
        }
        let store = GlobalStore::open(dir.path()).unwrap();
        assert!(store.has_asset("t3"), "the save after the repair replays");
        drop(store);

        // A flipped byte inside the last record: that save is lost, the
        // ones before it are not, and nothing pretends otherwise.
        let mut flipped = intact.clone();
        let last = flipped.len() - JOURNAL_SEAL_SIZE - 4;
        flipped[last] ^= 0x55;
        std::fs::write(&log, &flipped).unwrap();
        let store = GlobalStore::open(dir.path()).unwrap();
        assert!(store.has_asset("t0") && store.has_asset("t1"));
        assert!(!store.has_asset("t2"));
        assert!(!store.has_asset("t3"));
    }

    #[test]
    fn the_journal_rolls_into_a_snapshot_past_its_budget() {
        let dir = tempfile::tempdir().unwrap();
        let bodies: Vec<Vec<u8>> = (20u8..32).map(|i| vec![i; 200]).collect();
        let hashes: Vec<ChunkHash> = bodies.iter().map(|b| hash_chunk(b)).collect();
        let snapshot_generation;
        {
            let mut store = GlobalStore::open(dir.path()).unwrap();
            // Room for a few records, not for all of them.
            store.set_journal_budget(700);
            for (i, (body, hash)) in bodies.iter().zip(&hashes).enumerate() {
                store.put_chunk(hash, body, 0, body.len() as u32).unwrap();
                store
                    .publish_asset(&rec(&format!("r{i}"), &[hash]))
                    .unwrap();
            }
            let report = store.index_report();
            assert_eq!(report.generation, 12);
            assert!(report.snapshot_bytes > 0);
            assert!(
                report.journal_bytes <= 700,
                "journal stayed inside its budget: {}",
                report.journal_bytes
            );
            snapshot_generation =
                decode_index(&std::fs::read(dir.path().join("index.bin")).unwrap())
                    .unwrap()
                    .generation;
            assert!(snapshot_generation > 0 && snapshot_generation <= 12);
            assert!(dir.path().join("index.bin.prev").exists());
        }
        let store = GlobalStore::open(dir.path()).unwrap();
        assert_eq!(store.index_report().generation, 12);
        for i in 0..bodies.len() {
            assert!(store.has_asset(&format!("r{i}")), "r{i}");
        }
        drop(store);

        // Lose the live snapshot: the previous one plus the journal it had
        // rotated away recover every save up to the lost snapshot, and the
        // store says so through its generation rather than inventing one.
        std::fs::remove_file(dir.path().join("index.bin")).unwrap();
        let mut store = GlobalStore::open(dir.path()).unwrap();
        let recovered = store.index_report().generation;
        assert_eq!(recovered, snapshot_generation - 1);
        for i in 0..recovered as usize {
            assert!(store.has_asset(&format!("r{i}")), "r{i} recovered");
        }
        // The next save writes a snapshot rather than extending a recovered
        // ledger, and a reopen sees it.
        store.unpublish_asset("r0").unwrap();
        assert!(dir.path().join("index.bin").exists());
        drop(store);
        let store = GlobalStore::open(dir.path()).unwrap();
        assert!(!store.has_asset("r0"));
        assert_eq!(store.index_report().generation, recovered + 1);
    }

    #[test]
    fn a_publish_batch_writes_one_record_pack_and_reads_back_from_it() {
        let dir = tempfile::tempdir().unwrap();
        let bodies: Vec<Vec<u8>> = (40u8..45).map(|i| vec![i; 256]).collect();
        let hashes: Vec<ChunkHash> = bodies.iter().map(|b| hash_chunk(b)).collect();
        {
            let mut store =
                GlobalStore::open_with_layout(dir.path(), Some(StoreLayout::Packfiles)).unwrap();
            store.begin_publish_batch();
            for (i, (body, hash)) in bodies.iter().zip(&hashes).enumerate() {
                store.put_chunk(hash, body, 0, body.len() as u32).unwrap();
                let mut record = rec(&format!("p{i}"), &[hash]);
                record.meta.push(("i".into(), i.to_string()));
                store.publish_asset(&record).unwrap();
            }
            store.commit_publish_batch().unwrap();
        }
        let records_dir = dir.path().join("assets").join("records");
        let packs: Vec<_> = std::fs::read_dir(&records_dir).unwrap().flatten().collect();
        assert_eq!(packs.len(), 1, "one file for the batch");
        assert!(
            !dir.path().join("assets").join("p0.json").exists(),
            "no flat record per asset"
        );

        let store = GlobalStore::open(dir.path()).unwrap();
        for (i, hash) in hashes.iter().enumerate() {
            let record = store.get_asset(&format!("p{i}")).unwrap();
            assert_eq!(record.name, format!("p{i}"));
            assert_eq!(record.meta, vec![("i".to_string(), i.to_string())]);
            assert_eq!(record.chunk_table, vec![to_hex(hash)]);
        }
        assert!(matches!(
            store.get_asset("p9"),
            Err(StoreError::AssetNotFound(_))
        ));
        // An export reads the record where it lives.
        let out = tempfile::tempdir().unwrap();
        let written = store.export_asset("p3", out.path()).unwrap();
        assert!(written.contains(&"assets/p3/record.json".to_string()));
        let exported: AssetRecord = serde_json::from_slice(
            &std::fs::read(out.path().join("assets/p3/record.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(exported.meta, vec![("i".to_string(), "3".to_string())]);
    }

    #[test]
    fn replaced_and_unpublished_records_leave_packs_gc_reclaims() {
        let dir = tempfile::tempdir().unwrap();
        let a = vec![50u8; 300];
        let b = vec![51u8; 300];
        let (ha, hb) = (hash_chunk(&a), hash_chunk(&b));
        let mut store = GlobalStore::open(dir.path()).unwrap();
        store.put_chunk(&ha, &a, 0, 300).unwrap();
        store.put_chunk(&hb, &b, 0, 300).unwrap();
        store.publish_asset(&rec("x", &[&ha])).unwrap(); // pack 1
        store.publish_asset(&rec("y", &[&hb])).unwrap(); // pack 2
        store.publish_asset(&rec("x", &[&hb])).unwrap(); // pack 3 replaces x
        let records_dir = dir.path().join("assets").join("records");
        let count = || std::fs::read_dir(&records_dir).unwrap().flatten().count();
        assert_eq!(count(), 3);
        assert_eq!(store.get_asset("x").unwrap().chunk_table, vec![to_hex(&hb)]);
        assert!(store.unpublish_asset("y").unwrap());
        assert!(matches!(
            store.get_asset("y"),
            Err(StoreError::AssetNotFound(_))
        ));
        // Two packs hold nothing live; gc removes them and keeps x's.
        store.gc(0).unwrap();
        assert_eq!(count(), 1);
        drop(store);
        let store = GlobalStore::open(dir.path()).unwrap();
        assert_eq!(store.get_asset("x").unwrap().chunk_table, vec![to_hex(&hb)]);
    }

    #[test]
    fn a_flat_record_from_an_older_store_is_still_read_and_replaced_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let a = vec![60u8; 300];
        let ha = hash_chunk(&a);
        let mut store = GlobalStore::open(dir.path()).unwrap();
        store.put_chunk(&ha, &a, 0, 300).unwrap();
        store.publish_asset(&rec("old", &[&ha])).unwrap();
        // Make it look like a 1.7 publish: a flat file and no record entry.
        let flat = dir.path().join("assets").join("old.json");
        std::fs::write(
            &flat,
            serde_json::to_vec_pretty(&rec("old", &[&ha])).unwrap(),
        )
        .unwrap();
        store.index.records.remove("old");
        assert_eq!(store.get_asset("old").unwrap().name, "old");
        // Republishing moves it into a pack and drops the flat file.
        store.publish_asset(&rec("old", &[&ha])).unwrap();
        assert!(!flat.exists());
        assert_eq!(store.get_asset("old").unwrap().name, "old");
    }

    #[test]
    fn a_v1_snapshot_opens_and_its_first_save_rewrites_it_as_v2() {
        let dir = tempfile::tempdir().unwrap();
        let a = vec![70u8; 300];
        let ha = hash_chunk(&a);
        {
            let mut store = GlobalStore::open(dir.path()).unwrap();
            store.put_chunk(&ha, &a, 0, 300).unwrap();
            store.publish_asset(&rec("v1", &[&ha])).unwrap();
            // A flat record and a v1 snapshot, as 1.7 left them.
            let flat = dir.path().join("assets").join("v1.json");
            std::fs::write(
                &flat,
                serde_json::to_vec_pretty(&rec("v1", &[&ha])).unwrap(),
            )
            .unwrap();
            store.index.records.clear();
            let mut bytes = encode_index(&store.index);
            bytes[8..10].copy_from_slice(&1u16.to_le_bytes());
            // Strip the v2 location tags: one byte per asset, at the end.
            let body_len = bytes.len() - 32;
            let mut body = bytes[..body_len].to_vec();
            body.truncate(body_len - store.index.assets.len());
            let seal = cavs_hash::hash_chunk(&body);
            body.extend_from_slice(&seal);
            std::fs::write(dir.path().join("index.bin"), &body).unwrap();
            let _ = std::fs::remove_file(dir.path().join("index.log"));
        }
        let mut store = GlobalStore::open(dir.path()).unwrap();
        assert!(store.has_asset("v1"));
        assert_eq!(store.get_asset("v1").unwrap().name, "v1");
        assert_eq!(
            store.index_report().snapshot_bytes,
            0,
            "nothing to journal onto"
        );
        store
            .put_chunk(&hash_chunk(b"more"), b"more", 0, 4)
            .unwrap();
        store
            .publish_asset(&rec("v2", &[&hash_chunk(b"more")]))
            .unwrap();
        let (_, version) =
            decode_index_versioned(&std::fs::read(dir.path().join("index.bin")).unwrap()).unwrap();
        assert_eq!(version, 2);
        assert!(
            !dir.path().join("index.log").exists(),
            "the save was a snapshot"
        );
        assert!(store.index_report().snapshot_bytes > 0);
    }

    #[test]
    fn stale_tmp_snapshot_is_dropped_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let a = vec![3u8; 300];
        let ha = hash_chunk(&a);
        {
            let mut store = GlobalStore::open(dir.path()).unwrap();
            store.put_chunk(&ha, &a, 0, a.len() as u32).unwrap();
            store.publish_asset(&rec("keep", &[&ha])).unwrap();
        }
        // A crash mid-save leaves a partial staging file behind.
        std::fs::write(dir.path().join("index.bin.tmp"), b"half-written").unwrap();
        let store = GlobalStore::open(dir.path()).unwrap();
        assert!(store.has_asset("keep"));
        assert!(!dir.path().join("index.bin.tmp").exists());
    }

    #[test]
    fn future_index_version_is_rejected_with_clear_error() {
        let index = Index::default();
        let mut bytes = encode_index(&index);
        // Bump the header version and re-seal so only the version check trips.
        bytes[8..10].copy_from_slice(&99u16.to_le_bytes());
        let body_len = bytes.len() - 32;
        let seal = cavs_hash::hash_chunk(&bytes[..body_len]);
        bytes[body_len..].copy_from_slice(&seal);
        match decode_index(&bytes) {
            Err(StoreError::IndexCorrupt(msg)) => {
                assert!(msg.contains("newer"), "got: {msg}")
            }
            other => panic!("expected version rejection, got {other:?}"),
        }
    }

    #[test]
    fn quarantine_holds_packs_and_restores_referenced_ones() {
        let dir = tempfile::tempdir().unwrap();
        let mut store =
            GlobalStore::open_with_layout(dir.path(), Some(StoreLayout::Packfiles)).unwrap();
        let a = vec![9u8; 3000];
        let ha = hash_chunk(&a);
        store.put_chunk(&ha, &a, 0, a.len() as u32).unwrap();
        store.publish_asset(&rec("live", &[&ha])).unwrap();
        let pack_hex = store.chunk_info(&ha).unwrap().pack.clone().unwrap();
        let pack_path = packfile::pack_path(&store.packs_dir(), &pack_hex);

        // Quarantining a pack the ledger still references is recoverable:
        // the sweep notices and moves it straight back.
        store.quarantine_pack(&pack_hex).unwrap();
        assert!(!pack_path.exists());
        assert_eq!(store.sweep_quarantine(0).unwrap(), 0);
        assert!(pack_path.exists(), "referenced pack restored, not deleted");
        assert_eq!(store.verify().unwrap(), 1);

        // Same protection at open time (e.g. after a .prev ledger recovery).
        store.quarantine_pack(&pack_hex).unwrap();
        drop(store);
        let store = GlobalStore::open(dir.path()).unwrap();
        assert!(pack_path.exists(), "open restores quarantined live packs");
        assert_eq!(store.verify().unwrap(), 1);
    }

    #[test]
    fn orphan_packs_age_through_quarantine_before_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            GlobalStore::open_with_layout(dir.path(), Some(StoreLayout::Packfiles)).unwrap();
        let orphan_hex = "dead".to_owned() + &"be".repeat(30);
        let orphan = packfile::pack_path(&store.packs_dir(), &orphan_hex);
        std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
        std::fs::write(&orphan, b"orphaned pack bytes").unwrap();

        // Stage 1: past its grace period, the orphan is quarantined.
        store.quarantine_orphan_packs(0).unwrap();
        let qpack = dir.path().join(format!("quarantine/{orphan_hex}.cavspack"));
        assert!(!orphan.exists() && qpack.exists());

        // Still inside the quarantine period: nothing is deleted.
        assert_eq!(store.sweep_quarantine(3600).unwrap(), 0);
        assert!(qpack.exists());

        // Backdate the quarantine stamp: now the sweep may delete.
        std::fs::write(
            dir.path().join(format!("quarantine/{orphan_hex}.qsince")),
            "1",
        )
        .unwrap();
        assert_eq!(store.sweep_quarantine(3600).unwrap(), 19);
        assert!(!qpack.exists());
    }

    /// Scale probe for the ledger snapshot (not a correctness test):
    /// `cargo test -p cavs-store index_scale -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn index_scale_1m_chunks_bin_vs_json() {
        let mut index = Index {
            layout: StoreLayout::Packfiles,
            ..Index::default()
        };
        let n = 1_000_000u64;
        for i in 0..n {
            let h = hash_chunk(&i.to_le_bytes());
            index.chunks.insert(
                to_hex(&h),
                ChunkInfo {
                    len_raw: 65536,
                    len_stored: 60000,
                    flags: 1,
                    refcount: 2,
                    zero_since: None,
                    pack: Some(to_hex(&hash_chunk(&(i / 2048).to_le_bytes()))),
                    pack_offset: Some((i % 2048) * 60000),
                },
            );
        }
        let t = std::time::Instant::now();
        let bin = encode_index(&index);
        let t_enc = t.elapsed();
        let t = std::time::Instant::now();
        let back = decode_index(&bin).unwrap();
        let t_dec = t.elapsed();
        assert_eq!(back.chunks.len(), index.chunks.len());

        let t = std::time::Instant::now();
        let json = serde_json::to_vec_pretty(&index).unwrap();
        let t_jenc = t.elapsed();
        let t = std::time::Instant::now();
        let _: Index = serde_json::from_slice(&json).unwrap();
        let t_jdec = t.elapsed();

        println!("1M chunks:");
        println!(
            "  bin : {} bytes, encode {t_enc:?}, decode {t_dec:?}",
            bin.len()
        );
        println!(
            "  json: {} bytes, encode {t_jenc:?}, decode {t_jdec:?}",
            json.len()
        );
    }

    #[test]
    fn commit_publish_batch_without_batch_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let mut store =
            GlobalStore::open_with_layout(dir.path(), Some(StoreLayout::Packfiles)).unwrap();
        store.commit_publish_batch().unwrap();
        // Non-batched publishes still persist eagerly.
        let a = vec![9u8; 600];
        let ha = hash_chunk(&a);
        store.put_chunk(&ha, &a, 0, a.len() as u32).unwrap();
        store.publish_asset(&rec("solo", &[&ha])).unwrap();
        assert!(GlobalStore::open(dir.path())
            .unwrap()
            .get_asset("solo")
            .is_ok());
    }

    #[test]
    fn dedup_gc_and_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let a = vec![1u8; 1000];
        let b = vec![2u8; 1000];
        let c = vec![3u8; 1000];
        let (ha, hb, hc) = (hash_chunk(&a), hash_chunk(&b), hash_chunk(&c));

        {
            let mut store = GlobalStore::open(dir.path()).unwrap();
            // v1 = {a, b}
            assert!(store.put_chunk(&ha, &a, 0, a.len() as u32).unwrap());
            assert!(store.put_chunk(&hb, &b, 0, b.len() as u32).unwrap());
            store.publish_asset(&rec("game_v1", &[&ha, &hb])).unwrap();
            // v2 = {a, c}  — 'a' shared, stored once
            assert!(
                !store.put_chunk(&ha, &a, 0, a.len() as u32).unwrap(),
                "dup stored twice"
            );
            assert!(store.put_chunk(&hc, &c, 0, c.len() as u32).unwrap());
            store.publish_asset(&rec("game_v2", &[&ha, &hc])).unwrap();

            let s = store.stats();
            assert_eq!(s.assets, 2);
            assert_eq!(s.unique_chunks, 3); // a, b, c — not 4
            assert_eq!(store.chunk_info(&ha).unwrap().refcount, 2);
            // logical (both keep own copies) = 4 chunks; unique = 3
            assert_eq!(s.logical_stored_bytes, 4000);
            assert_eq!(s.stored_bytes, 3000);
        }

        // Reopen: index persisted.
        let mut store = GlobalStore::open(dir.path()).unwrap();
        assert_eq!(store.stats().unique_chunks, 3);
        assert!(store.get_asset("game_v1").is_ok());

        // Unpublish v1: 'b' drops to zero-ref, 'a' still referenced by v2.
        assert!(store.unpublish_asset("game_v1").unwrap());
        assert_eq!(store.chunk_info(&ha).unwrap().refcount, 1);
        assert_eq!(store.chunk_info(&hb).unwrap().refcount, 0);
        // GC with grace 0 reclaims 'b' only.
        let (removed, bytes) = store.gc(0).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(bytes, 1000);
        assert_eq!(store.stats().unique_chunks, 2);
        assert!(store.read_chunk_stored(&ha).is_ok());
        assert!(store.read_chunk_stored(&hb).is_err());
    }

    #[test]
    fn republish_replaces_refs() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = GlobalStore::open(dir.path()).unwrap();
        let a = vec![9u8; 500];
        let ha = hash_chunk(&a);
        store.put_chunk(&ha, &a, 0, 500).unwrap();
        store.publish_asset(&rec("x", &[&ha])).unwrap();
        store.publish_asset(&rec("x", &[&ha])).unwrap(); // republish
                                                         // refcount stays 1, not 2.
        assert_eq!(store.chunk_info(&ha).unwrap().refcount, 1);
    }

    #[test]
    fn missing_chunk_rejected_on_publish() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = GlobalStore::open(dir.path()).unwrap();
        let ghost = hash_chunk(b"never stored");
        assert!(matches!(
            store.publish_asset(&rec("x", &[&ghost])),
            Err(StoreError::MissingChunk(_))
        ));
    }

    fn packfile_store(dir: &Path) -> GlobalStore {
        let mut store = GlobalStore::open_with_layout(dir, Some(StoreLayout::Packfiles)).unwrap();
        store.set_preferred_pack_size(4 * 1000); // tiny packs: exercise rollover
        store
    }

    #[test]
    fn packfile_layout_roundtrip_rollover_and_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let chunks: Vec<Vec<u8>> = (0..10u8).map(|i| vec![i; 1000]).collect();
        let hashes: Vec<ChunkHash> = chunks.iter().map(|c| hash_chunk(c)).collect();
        {
            let mut store = packfile_store(dir.path());
            for (c, h) in chunks.iter().zip(&hashes) {
                assert!(store.put_chunk(h, c, 0, c.len() as u32).unwrap());
            }
            let refs: Vec<&ChunkHash> = hashes.iter().collect();
            store.publish_asset(&rec("app", &refs)).unwrap();

            // 10 KB of chunks at a 4 KB preferred size -> several packs.
            let stats = store.stats();
            assert_eq!(stats.layout, StoreLayout::Packfiles);
            assert!(stats.pack_count >= 2, "expected rollover: {stats:?}");
            assert_eq!(stats.pack_live_bytes, 10_000);
            // No loose chunk files were written.
            assert!(!dir
                .path()
                .join("chunks")
                .join(&to_hex(&hashes[0])[..2])
                .exists());
            store.verify().unwrap();
        }
        // Reopen: locations persisted; every chunk reads back identically.
        let store = GlobalStore::open(dir.path()).unwrap();
        assert_eq!(store.layout(), StoreLayout::Packfiles);
        for (c, h) in chunks.iter().zip(&hashes) {
            let (stored, _, _) = store.read_chunk_stored(h).unwrap();
            assert_eq!(&stored, c);
            assert!(store.chunk_location(h).is_some());
        }
    }

    #[test]
    fn coalesced_batch_read_matches_individual_reads() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = packfile_store(dir.path());
        store.set_preferred_pack_size(1 << 30); // one pack: adjacent chunks
        let chunks: Vec<Vec<u8>> = (0..50u8).map(|i| vec![i; 500]).collect();
        let hashes: Vec<ChunkHash> = chunks.iter().map(|c| hash_chunk(c)).collect();
        for (c, h) in chunks.iter().zip(&hashes) {
            store.put_chunk(h, c, 0, c.len() as u32).unwrap();
        }
        store.flush_packs().unwrap();

        // Request a scattered subset, out of order.
        let subset: Vec<ChunkHash> = [40usize, 2, 3, 4, 30, 31, 0]
            .iter()
            .map(|&i| hashes[i])
            .collect();
        let (batch, stats) = store.read_chunks_stored_batch(&subset).unwrap();
        for (got, &idx) in batch.iter().zip(&[40usize, 2, 3, 4, 30, 31, 0]) {
            assert_eq!(got.0, chunks[idx], "chunk {idx} mismatch");
        }
        // Adjacent chunks coalesce: fewer physical reads than chunks.
        assert_eq!(stats.pack_chunks_requested, 7);
        assert!(
            stats.pack_ranges_read < 7,
            "expected coalescing, got {stats:?}"
        );
        assert_eq!(stats.pack_bytes_served, 7 * 500);
        assert!(stats.pack_bytes_read >= stats.pack_bytes_served);
    }

    #[test]
    fn gc_deletes_only_fully_dead_packs() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = packfile_store(dir.path());
        // Pack 1: a+b (v1). Pack 2: c (v2, after explicit flush).
        let (a, b, c) = (vec![1u8; 1500], vec![2u8; 1500], vec![3u8; 1500]);
        let (ha, hb, hc) = (hash_chunk(&a), hash_chunk(&b), hash_chunk(&c));
        store.put_chunk(&ha, &a, 0, 1500).unwrap();
        store.put_chunk(&hb, &b, 0, 1500).unwrap();
        store.flush_packs().unwrap();
        store.put_chunk(&hc, &c, 0, 1500).unwrap();
        store.publish_asset(&rec("v1", &[&ha, &hb])).unwrap();
        store.publish_asset(&rec("v2", &[&hb, &hc])).unwrap();
        assert_eq!(store.stats().pack_count, 2);

        // Unpublish v2: 'c' dies; its pack holds only 'c' -> pack deleted.
        store.unpublish_asset("v2").unwrap();
        let (removed, bytes) = store.gc(0).unwrap();
        assert_eq!(removed, 1);
        assert!(bytes > 0, "dead pack must be reclaimed");
        assert_eq!(store.stats().pack_count, 1);
        assert!(store.read_chunk_stored(&hc).is_err());

        // Unpublish v1: 'a' and 'b' die, but they share the surviving pack
        // with nothing else -> that pack dies too.
        store.unpublish_asset("v1").unwrap();
        store.gc(0).unwrap();
        assert_eq!(store.stats().pack_count, 0);
        store.verify().unwrap();
    }

    #[test]
    fn layout_mismatch_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        drop(GlobalStore::open_with_layout(dir.path(), Some(StoreLayout::Packfiles)).unwrap());
        assert!(matches!(
            GlobalStore::open_with_layout(dir.path(), Some(StoreLayout::Loose)),
            Err(StoreError::LayoutMismatch { .. })
        ));
        // Re-opening without a requested layout keeps the stored one.
        assert_eq!(
            GlobalStore::open(dir.path()).unwrap().layout(),
            StoreLayout::Packfiles
        );
    }

    #[test]
    fn export_object_store_layout_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = packfile_store(dir.path());
        let data = vec![7u8; 2000];
        let h = hash_chunk(&data);
        store.put_chunk(&h, &data, 0, 2000).unwrap();
        store.publish_asset(&rec("app", &[&h])).unwrap();

        let out = dir.path().join("dist");
        let written = store.export_object_store(&out).unwrap();
        assert!(written.iter().any(|p| p.starts_with("chunks/packs/")));
        assert!(written.iter().any(|p| p.starts_with("chunks/indexes/")));
        assert!(written.contains(&"assets/app/record.json".to_string()));
        for rel in &written {
            assert!(out.join(rel).is_file(), "{rel} missing");
        }
        // Deterministic: exporting again yields the same paths.
        let out2 = dir.path().join("dist2");
        assert_eq!(written, store.export_object_store(&out2).unwrap());
        // Loose stores are not exportable.
        let loose_dir = tempfile::tempdir().unwrap();
        let loose = GlobalStore::open(loose_dir.path()).unwrap();
        assert!(matches!(
            loose.export_object_store(&out),
            Err(StoreError::NotExportable(_))
        ));
    }

    #[test]
    fn corrupted_pack_chunk_fails_verify() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = packfile_store(dir.path());
        let data = vec![9u8; 3000];
        let h = hash_chunk(&data);
        store.put_chunk(&h, &data, 0, 3000).unwrap();
        store.publish_asset(&rec("app", &[&h])).unwrap();
        store.verify().unwrap();

        // Flip one byte inside the pack's data region.
        let pack_hex = store.chunk_location(&h).unwrap().pack_hex;
        let pack = crate::packfile::pack_path(&dir.path().join("packs"), &pack_hex);
        let mut bytes = std::fs::read(&pack).unwrap();
        bytes[crate::packfile::PACK_HEADER_LEN as usize + 100] ^= 0xff;
        std::fs::write(&pack, &bytes).unwrap();
        assert!(store.verify().is_err(), "corruption must fail verify");
    }
}
