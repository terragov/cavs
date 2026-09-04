//! `cavs store` — manage a global content-addressable store: ingest `.cavs`
//! releases (deduplicating chunks across all of them), unpublish, garbage
//! collect zero-ref chunks, and report storage savings.

use crate::report::human_bytes;
use crate::StorageArg;
use anyhow::{Context, Result};
use cavs_format::{ingest_into_store, Reader};
use cavs_store::{GlobalStore, StoreLayout};
use std::path::Path;

/// Ingest a `.cavs` file into the store under `asset_name`, deduplicating
/// its chunks against everything already stored. `storage` selects the
/// physical layout when the store is newly created.
pub fn add(
    store_dir: &Path,
    asset_name: &str,
    cavs_path: &Path,
    storage: Option<StorageArg>,
) -> Result<()> {
    let mut reader =
        Reader::open(cavs_path).with_context(|| format!("cannot open {}", cavs_path.display()))?;

    let layout = storage.map(|s| match s {
        StorageArg::Loose => StoreLayout::Loose,
        StorageArg::Packfiles => StoreLayout::Packfiles,
    });
    let mut store = GlobalStore::open_with_layout(store_dir, layout)?;
    let stats = ingest_into_store(&mut reader, &mut store, asset_name)?;

    println!(
        "added   : {asset_name} ({} chunks, {} new / {} deduplicated)",
        stats.chunks,
        stats.new_chunks,
        stats.chunks - stats.new_chunks
    );
    println!(
        "new data: {} written to store",
        human_bytes(stats.new_bytes)
    );
    print_stats(&store);
    Ok(())
}

pub fn remove(store_dir: &Path, asset_name: &str) -> Result<()> {
    let mut store = GlobalStore::open(store_dir)?;
    if store.unpublish_asset(asset_name)? {
        println!(
            "removed : {asset_name} (run `cavs store {} gc` to reclaim space)",
            store_dir.display()
        );
        print_stats(&store);
    } else {
        println!("not found: {asset_name}");
    }
    Ok(())
}

pub fn gc(store_dir: &Path, grace_secs: u64) -> Result<()> {
    let mut store = GlobalStore::open(store_dir)?;
    let (removed, bytes) = store.gc(grace_secs)?;
    println!(
        "gc      : removed {removed} zero-ref chunks, reclaimed {}",
        human_bytes(bytes)
    );
    print_stats(&store);
    Ok(())
}

pub fn stat(store_dir: &Path) -> Result<()> {
    let store = GlobalStore::open(store_dir)?;
    println!("store   : {}", store_dir.display());
    for name in store.asset_names() {
        println!("  asset : {name}");
    }
    print_stats(&store);
    Ok(())
}

/// Re-hash every chunk and check every referenced pack's integrity.
pub fn verify(store_dir: &Path) -> Result<()> {
    let store = GlobalStore::open(store_dir)?;
    let checked = store.verify()?;
    println!("verify  : OK — {checked} chunks re-hashed, packs intact");
    Ok(())
}

/// Export as a deterministic immutable object tree for object storage/CDN.
pub fn export(store_dir: &Path, out: &Path, static_plans: bool) -> Result<()> {
    let store = GlobalStore::open(store_dir)?;
    let mut written = store.export_object_store(out)?;
    if static_plans {
        let plans = store.export_static_plans(out)?;
        let manifests = store.export_static_manifests(out)?;
        println!(
            "plans   : {} chunk-map.json + {} manifest.json (serverless clients)",
            plans.len(),
            manifests.len()
        );
        written.extend(plans);
        written.extend(manifests);
    }
    let packs = written
        .iter()
        .filter(|p| p.starts_with("chunks/packs/"))
        .count();
    println!(
        "exported: {} objects ({packs} packs) -> {}",
        written.len(),
        out.display()
    );
    println!("layout  :");
    for rel in written.iter().take(6) {
        println!("  {rel}");
    }
    if written.len() > 6 {
        println!("  … {} more", written.len() - 6);
    }
    println!(
        "headers : packs/indexes are content-addressed — serve them with\n          \
         Cache-Control: public, max-age=31536000, immutable\n          \
         ETag: \"blake3-<filename stem>\"\n          \
         assets/<name>/record.json is mutable — Cache-Control: no-cache"
    );
    Ok(())
}

/// Migrate the ledger to the segmented, mmapped index (Round 3B).
pub fn index_migrate(store_dir: &Path) -> Result<()> {
    let mut store = GlobalStore::open(store_dir)?;
    if store.is_segmented() {
        println!("index   : already segmented (nothing to do)");
        return Ok(());
    }
    let migrated = store.migrate_index_to_segmented()?;
    let report = store.index_report();
    println!(
        "migrated: {migrated} chunks -> segmented index (generation {}, {} segments)",
        report.generation, report.segments
    );
    println!("rollback: delete index/ and rename index.bin.pre-migration back");
    Ok(())
}

/// Report the ledger's index mode and structure.
pub fn index_inspect(store_dir: &Path) -> Result<()> {
    let store = GlobalStore::open(store_dir)?;
    let r = store.index_report();
    if r.segmented {
        println!(
            "index   : segmented · generation {} · {} segments ({} deltas pending compaction)",
            r.generation, r.segments, r.deltas
        );
    } else {
        println!(
            "index   : monolithic index.bin · generation {} · {} snapshot + {} journal (run `store index-migrate` to segment)",
            r.generation,
            human_bytes(r.snapshot_bytes),
            human_bytes(r.journal_bytes)
        );
    }
    print_stats(&store);
    Ok(())
}

/// Round 3D: fragmentation telemetry.
pub fn fragmentation(store_dir: &Path) -> Result<()> {
    let store = GlobalStore::open(store_dir)?;
    let f = store.fragmentation();
    println!(
        "packs   : {} total · {} small (<8 MiB, {:.0}%)",
        f.pack_count,
        f.small_packs,
        f.small_pack_ratio * 100.0
    );
    println!(
        "bytes   : {} on disk · {} live · {} dead ({:.1}%)",
        human_bytes(f.disk_bytes),
        human_bytes(f.live_bytes),
        human_bytes(f.dead_bytes),
        f.dead_bytes_ratio * 100.0
    );
    println!(
        "score   : {:.3} (small-pack ratio + dead-bytes ratio)",
        f.fragmentation_score
    );
    for p in f.packs.iter().take(8) {
        println!(
            "  pack {}… {} disk / {} live · {:.0}% dead · {} chunks",
            &p.pack[..12.min(p.pack.len())],
            human_bytes(p.disk_bytes),
            human_bytes(p.live_bytes),
            p.dead_ratio * 100.0,
            p.live_chunks
        );
    }
    if f.packs.len() > 8 {
        println!("  … {} more", f.packs.len() - 8);
    }
    Ok(())
}

/// Round 3D: merge small packs / compact dead bytes, copy-on-write.
pub fn repack(store_dir: &Path, dry_run: bool) -> Result<()> {
    let mut store = GlobalStore::open(store_dir)?;
    let plan = store.repack_plan();
    if plan.is_empty() {
        println!("repack  : nothing to do (no small packs, dead bytes under threshold)");
        return Ok(());
    }
    println!(
        "plan    : merge {} groups ({} packs) · compact {} packs · ~{} to read · ~{} reclaimable",
        plan.merge_groups.len(),
        plan.merge_groups.iter().map(Vec::len).sum::<usize>(),
        plan.compact_packs.len(),
        human_bytes(plan.estimated_read_bytes),
        human_bytes(plan.estimated_reclaim_bytes)
    );
    let outcome = store.repack_run(&plan, dry_run)?;
    if dry_run {
        println!(
            "dry-run : would rewrite {} packs / {} chunks ({} read)",
            outcome.packs_rewritten,
            outcome.chunks_moved,
            human_bytes(outcome.bytes_read)
        );
        return Ok(());
    }
    println!(
        "repack  : {} packs -> {} ({} chunks moved, {} read, {} written); {} quarantined",
        outcome.packs_rewritten,
        outcome.packs_written,
        outcome.chunks_moved,
        human_bytes(outcome.bytes_read),
        human_bytes(outcome.bytes_written),
        outcome.quarantined.len()
    );
    println!("note    : re-export affected assets if a static tree serves this store");
    print_stats(&store);
    Ok(())
}

fn print_stats(store: &GlobalStore) {
    let s = store.stats();
    // stored_bytes can briefly exceed the logical (referenced) total when
    // zero-ref orphans await gc, so saturate to avoid an underflow.
    let saved = if s.logical_stored_bytes == 0 {
        0.0
    } else {
        s.logical_stored_bytes.saturating_sub(s.stored_bytes) as f64 * 100.0
            / s.logical_stored_bytes as f64
    };
    println!(
        "totals  : {} assets · {} unique chunks · {} stored ({} zero-ref)",
        s.assets,
        s.unique_chunks,
        human_bytes(s.stored_bytes),
        s.zero_ref_chunks
    );
    println!(
        "dedup   : {} logical -> {} unique = {:.1}% storage saved across versions/titles",
        human_bytes(s.logical_stored_bytes),
        human_bytes(s.stored_bytes),
        saved
    );
    if s.layout == StoreLayout::Packfiles {
        let live_pct = if s.pack_disk_bytes == 0 {
            100.0
        } else {
            s.pack_live_bytes as f64 * 100.0 / s.pack_disk_bytes as f64
        };
        println!(
            "packs   : {} packfiles · {} on disk · {:.1}% live",
            s.pack_count,
            human_bytes(s.pack_disk_bytes),
            live_pct
        );
    }
}
