//! The graph walk, driven against a real store rather than a stub.

use std::collections::BTreeSet;
use std::time::Instant;

use cavs_object::{
    compute_missing, walk_reachable, DecodeLimits, FsObjectStore, HaveNothing, KindFilter,
    ObjectEnvelope, ObjectId, ObjectKind, ObjectStore, WalkOptions,
};

/// A store holding `chunks` payload objects of `chunk_len` bytes each, one
/// tree naming all of them, and one commit naming the tree.
fn build(dir: &std::path::Path, chunks: usize, chunk_len: usize) -> (FsObjectStore, ObjectId) {
    let store = FsObjectStore::open(dir).unwrap();
    let mut chunk_ids = Vec::with_capacity(chunks);
    for i in 0..chunks {
        let mut payload = vec![0u8; chunk_len];
        payload[..8].copy_from_slice(&(i as u64).to_le_bytes());
        chunk_ids.push(store.put_object(ObjectKind::Chunk, &payload).unwrap());
    }
    let tree = ObjectEnvelope::new(ObjectKind::Tree, chunk_ids, b"tree".to_vec()).unwrap();
    let tree_id = store.put_envelope(&tree).unwrap();
    let commit =
        ObjectEnvelope::new(ObjectKind::Commit, vec![tree_id], b"commit".to_vec()).unwrap();
    let commit_id = store.put_envelope(&commit).unwrap();
    (store, commit_id)
}

#[test]
fn a_walk_over_a_real_store_reaches_everything() {
    let dir = tempfile::tempdir().unwrap();
    let (store, root) = build(dir.path(), 64, 4096);
    let visits = walk_reachable(&store, &[root], WalkOptions::default()).unwrap();
    assert_eq!(visits.len(), 66, "commit + tree + 64 chunks");
    assert!(visits.iter().all(|v| v.is_present()));
    let bytes: u64 = visits.iter().map(|v| v.stored_len()).sum();
    assert!(bytes >= 64 * 4096);
}

#[test]
fn a_metadata_walk_stops_before_the_payload() {
    let dir = tempfile::tempdir().unwrap();
    let (store, root) = build(dir.path(), 64, 4096);
    let visits = walk_reachable(&store, &[root], WalkOptions::metadata_only()).unwrap();
    assert_eq!(visits.len(), 2, "commit and tree only");
    assert!(visits
        .iter()
        .all(|v| v.node.as_ref().unwrap().kind != ObjectKind::Chunk));
}

/// Reading a chunk's edges must not read the chunk. The store answers a
/// payload lookup from the header and the file size, so a full walk over
/// megabytes of payload stays as cheap as a metadata walk over the same graph.
///
/// The assertion is deliberately loose — wall-clock on a shared machine is
/// noisy — but the ratio it would catch is the one that matters: reading every
/// payload byte instead of stat-ing it is orders of magnitude, not percent.
#[test]
fn walking_payload_does_not_read_payload() {
    let dir = tempfile::tempdir().unwrap();
    // 256 chunks × 256 KiB = 64 MiB of payload.
    let (store, root) = build(dir.path(), 256, 256 * 1024);

    let started = Instant::now();
    let full = walk_reachable(&store, &[root], WalkOptions::default()).unwrap();
    let full_walk = started.elapsed();

    let started = Instant::now();
    let mut read_bytes = 0u64;
    for visit in &full {
        if visit.node.as_ref().unwrap().kind == ObjectKind::Chunk {
            read_bytes += store.get_object(&visit.id).unwrap().bytes.len() as u64;
        }
    }
    let full_read = started.elapsed();

    eprintln!(
        "walk over 64 MiB of payload: {full_walk:?}; reading the same payload: {full_read:?}"
    );
    assert_eq!(read_bytes, 256 * 256 * 1024);
    assert!(
        full_walk < full_read,
        "walking {full_walk:?} was not cheaper than reading {full_read:?}"
    );
}

#[test]
fn a_receiver_that_has_the_tree_needs_only_the_commit() {
    let dir = tempfile::tempdir().unwrap();
    let (store, root) = build(dir.path(), 8, 1024);
    let tree = store
        .get_object(&root)
        .unwrap()
        .dependencies(DecodeLimits::DEFAULT)
        .unwrap()[0];

    let have: BTreeSet<ObjectId> = [tree].into_iter().collect();
    let plan = compute_missing(&store, &[root], &have, WalkOptions::default()).unwrap();
    assert_eq!(plan.missing, vec![root]);
    assert_eq!(plan.already_present, 1);
    assert!(plan.is_fulfillable());
}

#[test]
fn a_receiver_that_has_the_root_needs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (store, root) = build(dir.path(), 8, 1024);
    let have: BTreeSet<ObjectId> = [root].into_iter().collect();
    let plan = compute_missing(&store, &[root], &have, WalkOptions::default()).unwrap();
    assert!(plan.is_empty());
    assert_eq!(plan.bytes, 0);
}

#[test]
fn a_metadata_only_plan_leaves_the_payload_behind() {
    let dir = tempfile::tempdir().unwrap();
    let (store, root) = build(dir.path(), 32, 8192);
    let plan = compute_missing(
        &store,
        &[root],
        &HaveNothing,
        WalkOptions::default().with_kinds(KindFilter::metadata_only()),
    )
    .unwrap();
    assert_eq!(plan.missing.len(), 2);
    assert!(plan.bytes < 32 * 8192);
}

/// An object whose dependency was deleted is a gap, not corruption: the walk
/// names it and the plan says the source cannot fulfil it on its own.
#[test]
fn a_deleted_dependency_shows_up_as_a_gap() {
    let dir = tempfile::tempdir().unwrap();
    let (store, root) = build(dir.path(), 4, 512);
    let tree = store
        .get_object(&root)
        .unwrap()
        .dependencies(DecodeLimits::DEFAULT)
        .unwrap()[0];
    let victim = store
        .get_object(&tree)
        .unwrap()
        .dependencies(DecodeLimits::DEFAULT)
        .unwrap()[0];
    assert!(store.remove_object(&victim).unwrap());

    let visits = walk_reachable(&store, &[root], WalkOptions::default()).unwrap();
    let gap = visits.iter().find(|v| v.id == victim).unwrap();
    assert!(!gap.is_present());

    let plan = compute_missing(&store, &[root], &HaveNothing, WalkOptions::default()).unwrap();
    assert!(!plan.is_fulfillable());
    assert_eq!(plan.unavailable, vec![victim]);
}
