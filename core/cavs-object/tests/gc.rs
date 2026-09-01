//! Garbage collection: what it removes, and everything it must not.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use cavs_object::{
    collect, FsObjectStore, GcOptions, ObjectEnvelope, ObjectId, ObjectKind, ObjectStore,
};

fn store(dir: &tempfile::TempDir) -> FsObjectStore {
    FsObjectStore::open(dir.path().join("objects")).unwrap()
}

/// commit -> tree -> chunks. Returns the commit.
fn write_generation(store: &FsObjectStore, label: &str, chunks: usize) -> ObjectId {
    let mut chunk_ids = Vec::new();
    for i in 0..chunks {
        chunk_ids.push(
            store
                .put_object(ObjectKind::Chunk, format!("{label} chunk {i}").as_bytes())
                .unwrap(),
        );
    }
    let tree = store
        .put_envelope(
            &ObjectEnvelope::new(
                ObjectKind::Tree,
                chunk_ids,
                format!("{label} tree").into_bytes(),
            )
            .unwrap(),
        )
        .unwrap();
    store
        .put_envelope(
            &ObjectEnvelope::new(
                ObjectKind::Commit,
                vec![tree],
                format!("{label} commit").into_bytes(),
            )
            .unwrap(),
        )
        .unwrap()
}

/// A grace period of zero, so a test does not have to wait two hours to see
/// anything collected.
fn no_grace() -> GcOptions {
    GcOptions::default().with_grace(Duration::ZERO)
}

#[test]
fn nothing_reachable_is_ever_removed() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let root = write_generation(&store, "kept", 8);
    let all = store.list_objects().unwrap();

    let roots = vec![root];
    let report = collect(&store, &|| Ok(roots.clone()), &no_grace()).unwrap();

    assert_eq!(report.reachable, 10);
    assert!(report.removed.is_empty());
    assert_eq!(store.list_objects().unwrap(), all);
}

#[test]
fn an_orphan_is_removed_once_the_grace_period_has_passed() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let kept = write_generation(&store, "kept", 4);
    let orphan = write_generation(&store, "orphan", 4);

    let roots = vec![kept];
    let report = collect(&store, &|| Ok(roots.clone()), &no_grace()).unwrap();

    assert_eq!(
        report.removed_count(),
        6,
        "the orphan commit, tree and chunks"
    );
    assert!(report.bytes_removed > 0);
    assert!(!store.has_object(&orphan).unwrap());
    assert!(store.has_object(&kept).unwrap());
    // And what stayed is still readable, not merely present.
    assert!(store.verify_all().unwrap().is_clean());
}

/// The case a plain mark-and-sweep gets wrong: a writer has written its
/// objects and has not yet published the reference that names them.
#[test]
fn a_transaction_in_flight_is_not_collected() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let published = write_generation(&store, "published", 4);
    // Written a moment ago, referenced by nothing yet.
    let in_flight = write_generation(&store, "in flight", 4);

    let roots = vec![published];
    let report = collect(&store, &|| Ok(roots.clone()), &GcOptions::default()).unwrap();

    assert!(report.removed.is_empty());
    assert_eq!(report.kept_by_grace, 6);
    assert!(store.has_object(&in_flight).unwrap());
}

#[test]
fn a_leased_object_is_kept_even_past_the_grace_period() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let kept = write_generation(&store, "kept", 2);
    let claimed = store
        .put_object(ObjectKind::Chunk, b"a reader is using this")
        .unwrap();

    let roots = vec![kept];
    let report = collect(
        &store,
        &|| Ok(roots.clone()),
        &no_grace().with_leases([claimed]),
    )
    .unwrap();

    assert_eq!(report.kept_by_lease, 1);
    assert!(report.removed.is_empty());
    assert!(store.has_object(&claimed).unwrap());
}

#[test]
fn a_dry_run_changes_nothing_and_says_the_same_thing() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let kept = write_generation(&store, "kept", 4);
    write_generation(&store, "orphan", 4);
    let before = store.list_objects().unwrap();

    let roots = vec![kept];
    let planned = collect(
        &store,
        &|| Ok(roots.clone()),
        &GcOptions {
            dry_run: true,
            ..no_grace()
        },
    )
    .unwrap();
    assert!(planned.dry_run);
    assert_eq!(planned.removed_count(), 6);
    assert_eq!(store.list_objects().unwrap(), before, "a dry run deleted");

    let done = collect(&store, &|| Ok(roots.clone()), &no_grace()).unwrap();
    assert_eq!(done.removed, planned.removed, "the two runs disagreed");
}

/// A collection that races a commit has to lose the race, not the data.
#[test]
fn a_collection_that_sees_the_roots_move_deletes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let first = write_generation(&store, "first", 4);
    let second = write_generation(&store, "second", 4);
    let before = store.list_objects().unwrap();

    // The reference store answers differently the second time it is asked,
    // which is exactly what a concurrent commit looks like from here.
    let calls = AtomicUsize::new(0);
    let report = collect(
        &store,
        &|| {
            let call = calls.fetch_add(1, Ordering::Relaxed);
            Ok(if call == 0 {
                vec![first]
            } else {
                vec![first, second]
            })
        },
        &no_grace(),
    )
    .unwrap();

    assert!(report.aborted_because_roots_moved);
    assert!(report.removed.is_empty());
    assert_eq!(store.list_objects().unwrap(), before);
}

/// Two branches sharing most of their objects: deleting one must not touch
/// what the other still needs.
#[test]
fn objects_shared_between_roots_survive_losing_one_of_them() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);

    let mut chunks = Vec::new();
    for i in 0..16 {
        chunks.push(
            store
                .put_object(ObjectKind::Chunk, format!("shared {i}").as_bytes())
                .unwrap(),
        );
    }
    let shared_tree = store
        .put_envelope(
            &ObjectEnvelope::new(ObjectKind::Tree, chunks.clone(), b"t".to_vec()).unwrap(),
        )
        .unwrap();
    let keep = store
        .put_envelope(
            &ObjectEnvelope::new(ObjectKind::Commit, vec![shared_tree], b"keep".to_vec()).unwrap(),
        )
        .unwrap();

    // A second branch over the same chunks plus one of its own.
    let extra = store.put_object(ObjectKind::Chunk, b"only mine").unwrap();
    let mut with_extra = chunks.clone();
    with_extra.push(extra);
    let other_tree = store
        .put_envelope(&ObjectEnvelope::new(ObjectKind::Tree, with_extra, b"t2".to_vec()).unwrap())
        .unwrap();
    let drop_me = store
        .put_envelope(
            &ObjectEnvelope::new(ObjectKind::Commit, vec![other_tree], b"drop".to_vec()).unwrap(),
        )
        .unwrap();

    let roots = vec![keep];
    let report = collect(&store, &|| Ok(roots.clone()), &no_grace()).unwrap();

    assert_eq!(
        report.removed_count(),
        3,
        "only the dropped branch's own commit, tree and chunk"
    );
    assert!(!store.has_object(&drop_me).unwrap());
    assert!(!store.has_object(&extra).unwrap());
    for chunk in &chunks {
        assert!(
            store.has_object(chunk).unwrap(),
            "a shared chunk was removed"
        );
    }
    assert!(store.has_object(&shared_tree).unwrap());
}

#[test]
fn a_root_that_is_not_in_the_store_is_reported_not_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let real = write_generation(&store, "real", 2);
    let ghost = ObjectId::from_blake3([0x5a; 32]);

    let roots = vec![real, ghost];
    let report = collect(&store, &|| Ok(roots.clone()), &no_grace()).unwrap();
    assert_eq!(report.missing_roots, vec![ghost]);
    // And a broken reference does not become a reason to collect the rest.
    assert!(store.has_object(&real).unwrap());
}

#[test]
fn collecting_an_empty_store_is_uneventful() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let report = collect(&store, &|| Ok(Vec::new()), &no_grace()).unwrap();
    assert_eq!(report.roots, 0);
    assert_eq!(report.reachable, 0);
    assert!(report.removed.is_empty());
}

/// Collection under concurrent writes: whatever a writer publishes during the
/// sweep must still be there afterwards.
#[test]
fn writers_running_alongside_a_collection_keep_their_objects() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(&dir);
    let kept = write_generation(&store, "kept", 4);
    write_generation(&store, "orphan", 32);

    let published: Mutex<Vec<ObjectId>> = Mutex::new(vec![kept]);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for i in 0..16 {
                let id = write_generation(&store, &format!("live {i}"), 2);
                published.lock().unwrap().push(id);
            }
        });
        scope.spawn(|| {
            for _ in 0..4 {
                let _ = collect(
                    &store,
                    &|| Ok(published.lock().unwrap().clone()),
                    &GcOptions::default(),
                );
            }
        });
    });

    for id in published.lock().unwrap().iter() {
        assert!(
            store.has_object(id).unwrap(),
            "a published commit was collected"
        );
    }
    assert!(store.verify_all().unwrap().is_clean());
}
