//! Bundles: creating, inspecting, verifying, importing — and refusing.

use std::collections::BTreeSet;

use cavs_object::{
    create_bundle, import_bundle, inspect_bundle, verify_bundle, BundleLimits, BundleOptions,
    BundleRef, DecodeLimits, FsObjectStore, ObjectEnvelope, ObjectId, ObjectKind, ObjectStore,
    WalkOptions,
};

/// A store holding commit -> tree -> chunks, and the commit's id.
fn build(dir: &std::path::Path, chunks: usize) -> (FsObjectStore, ObjectId) {
    let store = FsObjectStore::open(dir).unwrap();
    let mut chunk_ids = Vec::new();
    for i in 0..chunks {
        let payload = format!("chunk payload number {i} with some bytes to make it real");
        chunk_ids.push(
            store
                .put_object(ObjectKind::Chunk, payload.as_bytes())
                .unwrap(),
        );
    }
    let tree = ObjectEnvelope::new(ObjectKind::Tree, chunk_ids, b"tree body".to_vec()).unwrap();
    let tree_id = store.put_envelope(&tree).unwrap();
    let commit =
        ObjectEnvelope::new(ObjectKind::Commit, vec![tree_id], b"commit body".to_vec()).unwrap();
    let commit_id = store.put_envelope(&commit).unwrap();
    (store, commit_id)
}

fn empty_store(dir: &std::path::Path) -> FsObjectStore {
    FsObjectStore::open(dir).unwrap()
}

#[test]
fn a_bundle_round_trips_into_an_empty_store() {
    let source_dir = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let out = source_dir.path().join("repo.cavsbundle");
    let (source, root) = build(&source_dir.path().join("objects"), 16);

    let summary = create_bundle(
        &source,
        &[(ObjectKind::Commit, root)],
        &out,
        &BundleOptions::default(),
    )
    .unwrap();
    assert_eq!(summary.objects, 18, "commit, tree and sixteen chunks");
    assert_eq!(summary.prerequisites, 0);

    let target = empty_store(&target_dir.path().join("objects"));
    let report = import_bundle(&out, &target, &BundleLimits::default()).unwrap();
    assert_eq!(report.objects_added, 18);
    assert!(report.is_complete());

    // Same ids on both sides, which is the whole point.
    assert_eq!(
        source.list_objects().unwrap(),
        target.list_objects().unwrap()
    );
    assert_eq!(
        target.get_object(&root).unwrap().bytes,
        source.get_object(&root).unwrap().bytes
    );
}

#[test]
fn importing_twice_changes_nothing_the_second_time() {
    let source_dir = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let out = source_dir.path().join("repo.cavsbundle");
    let (source, root) = build(&source_dir.path().join("objects"), 8);
    create_bundle(
        &source,
        &[(ObjectKind::Commit, root)],
        &out,
        &BundleOptions::default(),
    )
    .unwrap();

    let target = empty_store(&target_dir.path().join("objects"));
    let first = import_bundle(&out, &target, &BundleLimits::default()).unwrap();
    let after_first = target.list_objects().unwrap();
    let second = import_bundle(&out, &target, &BundleLimits::default()).unwrap();

    assert_eq!(first.objects_added, 10);
    assert_eq!(second.objects_added, 0);
    assert_eq!(second.objects_already_present, 10);
    assert_eq!(target.list_objects().unwrap(), after_first);
}

#[test]
fn inspecting_reads_the_header_without_the_objects() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("repo.cavsbundle");
    let (store, root) = build(&dir.path().join("objects"), 4);
    create_bundle(
        &store,
        &[(ObjectKind::Commit, root)],
        &out,
        &BundleOptions::default().with_refs(vec![BundleRef {
            name: "refs/heads/main".into(),
            target: root,
        }]),
    )
    .unwrap();

    let info = inspect_bundle(&out, &BundleLimits::default()).unwrap();
    assert_eq!(info.roots, vec![(ObjectKind::Commit, root)]);
    assert_eq!(info.refs.len(), 1);
    assert_eq!(info.refs[0].name, "refs/heads/main");
    assert_eq!(info.object_count, 6);
    assert!(!info.is_thin());
    assert!(!info.compressed);
}

#[test]
fn a_verified_bundle_reports_what_it_checked() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("repo.cavsbundle");
    let (store, root) = build(&dir.path().join("objects"), 12);
    create_bundle(
        &store,
        &[(ObjectKind::Commit, root)],
        &out,
        &BundleOptions::default(),
    )
    .unwrap();

    let verification = verify_bundle(&out, &BundleLimits::default()).unwrap();
    assert!(verification.is_complete());
    assert_eq!(verification.objects_verified, 14);
    assert!(verification.bytes_verified > 0);
}

/// A bundle that was edited on the way must fail while the store is still
/// exactly as it was.
#[test]
fn an_altered_bundle_fails_before_anything_is_published() {
    let source_dir = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let out = source_dir.path().join("repo.cavsbundle");
    let (source, root) = build(&source_dir.path().join("objects"), 8);
    create_bundle(
        &source,
        &[(ObjectKind::Commit, root)],
        &out,
        &BundleOptions::default(),
    )
    .unwrap();

    let mut raw = std::fs::read(&out).unwrap();
    let middle = raw.len() / 2;
    raw[middle] ^= 0xff;
    std::fs::write(&out, &raw).unwrap();

    let target = empty_store(&target_dir.path().join("objects"));
    assert!(verify_bundle(&out, &BundleLimits::default()).is_err());
    assert!(import_bundle(&out, &target, &BundleLimits::default()).is_err());
    assert_eq!(
        target.object_count().unwrap(),
        0,
        "the store was written to"
    );
}

#[test]
fn a_truncated_bundle_is_refused_at_every_length() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("repo.cavsbundle");
    let (store, root) = build(&dir.path().join("objects"), 4);
    create_bundle(
        &store,
        &[(ObjectKind::Commit, root)],
        &out,
        &BundleOptions::default(),
    )
    .unwrap();
    let whole = std::fs::read(&out).unwrap();

    let cut_path = dir.path().join("cut.cavsbundle");
    for cut in (0..whole.len()).step_by(7) {
        std::fs::write(&cut_path, &whole[..cut]).unwrap();
        assert!(
            inspect_bundle(&cut_path, &BundleLimits::default()).is_err(),
            "a bundle truncated to {cut} bytes was accepted"
        );
    }
}

/// A thin bundle carries only what the receiver lacks, and says what it is
/// assuming. Imported against a store that has the prerequisites it completes;
/// against one that does not, it says exactly what is missing rather than
/// leaving a gap to be discovered later.
#[test]
fn a_thin_bundle_carries_only_the_difference() {
    let source_dir = tempfile::tempdir().unwrap();
    let out_full = source_dir.path().join("full.cavsbundle");
    let out_thin = source_dir.path().join("thin.cavsbundle");
    let (source, first_root) = build(&source_dir.path().join("objects"), 16);

    // The source grows: a second commit over a tree with one more chunk.
    let extra = source
        .put_object(ObjectKind::Chunk, b"a chunk only the new commit has")
        .unwrap();
    let first_tree = source
        .get_object(&first_root)
        .unwrap()
        .dependencies(DecodeLimits::DEFAULT)
        .unwrap()[0];
    let mut chunks = source
        .get_object(&first_tree)
        .unwrap()
        .dependencies(DecodeLimits::DEFAULT)
        .unwrap();
    chunks.push(extra);
    let second_tree = source
        .put_envelope(&ObjectEnvelope::new(ObjectKind::Tree, chunks, b"tree two".to_vec()).unwrap())
        .unwrap();
    let second_root = source
        .put_envelope(
            &ObjectEnvelope::new(
                ObjectKind::Commit,
                vec![second_tree, first_root],
                b"commit two".to_vec(),
            )
            .unwrap(),
        )
        .unwrap();

    create_bundle(
        &source,
        &[(ObjectKind::Commit, first_root)],
        &out_full,
        &BundleOptions::default(),
    )
    .unwrap();

    // What a receiver that already has the first commit is missing.
    let already: BTreeSet<ObjectId> = {
        let receiver_dir = tempfile::tempdir().unwrap();
        let receiver = empty_store(&receiver_dir.path().join("objects"));
        import_bundle(&out_full, &receiver, &BundleLimits::default()).unwrap();
        receiver.list_objects().unwrap().into_iter().collect()
    };

    let thin = create_bundle(
        &source,
        &[(ObjectKind::Commit, second_root)],
        &out_thin,
        &BundleOptions::default().excluding(already.clone()),
    )
    .unwrap();
    assert_eq!(thin.objects, 3, "the new chunk, tree and commit");
    assert!(thin.prerequisites > 0);

    let full_size = std::fs::metadata(&out_full).unwrap().len();
    let thin_size = std::fs::metadata(&out_thin).unwrap().len();
    assert!(
        thin_size < full_size,
        "the thin bundle is {thin_size} against {full_size} for the full one"
    );
    // The prerequisites are the frontier the new tree names, not the whole
    // closure the receiver already holds.
    let info = inspect_bundle(&out_thin, &BundleLimits::default()).unwrap();
    assert!(
        info.prerequisites.len() <= 17,
        "a thin bundle named {} prerequisites",
        info.prerequisites.len()
    );

    // Against a store that has the prerequisites, it completes.
    let ready_dir = tempfile::tempdir().unwrap();
    let ready = empty_store(&ready_dir.path().join("objects"));
    import_bundle(&out_full, &ready, &BundleLimits::default()).unwrap();
    let report = import_bundle(&out_thin, &ready, &BundleLimits::default()).unwrap();
    assert!(report.is_complete());
    assert_eq!(report.objects_added, 3);
    assert!(ready.has_object(&second_root).unwrap());

    // Against an empty one, it says what it needed.
    let bare_dir = tempfile::tempdir().unwrap();
    let bare = empty_store(&bare_dir.path().join("objects"));
    let report = import_bundle(&out_thin, &bare, &BundleLimits::default()).unwrap();
    assert!(!report.is_complete());
    assert!(!report.missing_prerequisites.is_empty());
}

#[test]
fn a_metadata_only_bundle_leaves_the_payload_behind() {
    let dir = tempfile::tempdir().unwrap();
    let full = dir.path().join("full.cavsbundle");
    let metadata = dir.path().join("metadata.cavsbundle");
    let (store, root) = build(&dir.path().join("objects"), 64);

    create_bundle(
        &store,
        &[(ObjectKind::Commit, root)],
        &full,
        &BundleOptions::default(),
    )
    .unwrap();
    let summary = create_bundle(
        &store,
        &[(ObjectKind::Commit, root)],
        &metadata,
        &BundleOptions {
            walk: Some(WalkOptions::metadata_only()),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(summary.objects, 2, "commit and tree only");
    assert!(std::fs::metadata(&metadata).unwrap().len() < std::fs::metadata(&full).unwrap().len());

    // It is honest about being incomplete: the chunks the tree names are
    // neither inside it nor declared.
    let verification = verify_bundle(&metadata, &BundleLimits::default()).unwrap();
    assert!(!verification.is_complete());
    assert_eq!(verification.dangling.len(), 64);
}

#[test]
fn compression_shrinks_a_bundle_without_changing_what_comes_out() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain.cavsbundle");
    let squeezed = dir.path().join("squeezed.cavsbundle");
    let (store, root) = build(&dir.path().join("objects"), 200);

    create_bundle(
        &store,
        &[(ObjectKind::Commit, root)],
        &plain,
        &BundleOptions::default(),
    )
    .unwrap();
    create_bundle(
        &store,
        &[(ObjectKind::Commit, root)],
        &squeezed,
        &BundleOptions::default().compressed(),
    )
    .unwrap();

    let plain_size = std::fs::metadata(&plain).unwrap().len();
    let squeezed_size = std::fs::metadata(&squeezed).unwrap().len();
    assert!(
        squeezed_size < plain_size,
        "compressed {squeezed_size} was not smaller than plain {plain_size}"
    );

    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let from_plain = empty_store(&a.path().join("objects"));
    let from_squeezed = empty_store(&b.path().join("objects"));
    import_bundle(&plain, &from_plain, &BundleLimits::default()).unwrap();
    import_bundle(&squeezed, &from_squeezed, &BundleLimits::default()).unwrap();
    assert_eq!(
        from_plain.list_objects().unwrap(),
        from_squeezed.list_objects().unwrap()
    );
}

/// A file claiming a huge decompressed size from a tiny compressed one is the
/// standard way to turn a download into an out-of-memory kill.
#[test]
fn a_bundle_that_claims_absurd_expansion_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("repo.cavsbundle");
    let (store, root) = build(&dir.path().join("objects"), 4);
    create_bundle(
        &store,
        &[(ObjectKind::Commit, root)],
        &out,
        &BundleOptions::default().compressed(),
    )
    .unwrap();

    let tight = BundleLimits {
        max_payload_bytes: 16,
        ..Default::default()
    };
    let err = inspect_bundle(&out, &tight).unwrap_err();
    assert!(err.to_string().contains("payload"), "{err}");
}

#[test]
fn a_bundle_of_nothing_is_still_a_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("empty.cavsbundle");
    let store = empty_store(&dir.path().join("objects"));
    let summary = create_bundle(&store, &[], &out, &BundleOptions::default()).unwrap();
    assert_eq!(summary.objects, 0);

    let info = inspect_bundle(&out, &BundleLimits::default()).unwrap();
    assert!(info.roots.is_empty());
    assert!(verify_bundle(&out, &BundleLimits::default())
        .unwrap()
        .is_complete());

    let target_dir = tempfile::tempdir().unwrap();
    let target = empty_store(&target_dir.path().join("objects"));
    let report = import_bundle(&out, &target, &BundleLimits::default()).unwrap();
    assert_eq!(report.objects_added, 0);
}

#[test]
fn a_bundle_of_a_root_that_is_not_there_fails_at_creation() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("repo.cavsbundle");
    let store = empty_store(&dir.path().join("objects"));
    let ghost = ObjectId::from_blake3([4; 32]);
    assert!(create_bundle(
        &store,
        &[(ObjectKind::Commit, ghost)],
        &out,
        &BundleOptions::default()
    )
    .is_err());
}

#[test]
fn a_file_that_is_not_a_bundle_is_told_so() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("not-a-bundle");
    std::fs::write(&out, vec![0u8; 4096]).unwrap();
    let err = inspect_bundle(&out, &BundleLimits::default()).unwrap_err();
    assert!(err.to_string().contains("bundle"), "{err}");
}

#[test]
fn a_bundle_can_be_signed_after_it_is_written() {
    use cavs_object::{
        append_signature, bundle_content_checksum, sign_bundle_checksum, verify_signatures, KeyRing,
    };
    use ed25519_dalek::SigningKey;

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("repo.cavsbundle");
    let (store, root) = build(&dir.path().join("objects"), 8);
    create_bundle(
        &store,
        &[(ObjectKind::Commit, root)],
        &out,
        &BundleOptions::default(),
    )
    .unwrap();

    let publisher = SigningKey::from_bytes(&[7u8; 32]);
    let checksum = bundle_content_checksum(&out, &BundleLimits::default()).unwrap();
    append_signature(
        &out,
        sign_bundle_checksum(&publisher, &checksum),
        &BundleLimits::default(),
    )
    .unwrap();

    // Still a valid bundle, and still imports.
    let info = inspect_bundle(&out, &BundleLimits::default()).unwrap();
    assert_eq!(info.signatures.len(), 1);
    assert!(verify_bundle(&out, &BundleLimits::default())
        .unwrap()
        .is_complete());

    let mut ring = KeyRing::new();
    ring.trust(publisher.verifying_key());
    let check = verify_signatures(
        &cavs_object::sign::bundle_message(&checksum),
        &info.signatures,
        &ring,
    );
    assert!(check.is_trusted());
}

/// A second publisher signing must not invalidate the first one's signature:
/// the checksum they both sign stops before the signature block.
#[test]
fn two_publishers_can_sign_one_bundle() {
    use cavs_object::{
        append_signature, bundle_content_checksum, sign_bundle_checksum, verify_signatures, KeyRing,
    };
    use ed25519_dalek::SigningKey;

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("repo.cavsbundle");
    let (store, root) = build(&dir.path().join("objects"), 4);
    create_bundle(
        &store,
        &[(ObjectKind::Commit, root)],
        &out,
        &BundleOptions::default(),
    )
    .unwrap();

    let checksum = bundle_content_checksum(&out, &BundleLimits::default()).unwrap();
    let mut ring = KeyRing::new();
    for seed in [1u8, 2] {
        let key = SigningKey::from_bytes(&[seed; 32]);
        ring.trust(key.verifying_key());
        append_signature(
            &out,
            sign_bundle_checksum(&key, &checksum),
            &BundleLimits::default(),
        )
        .unwrap();
    }

    // The checksum did not move under them.
    assert_eq!(
        bundle_content_checksum(&out, &BundleLimits::default()).unwrap(),
        checksum
    );
    let info = inspect_bundle(&out, &BundleLimits::default()).unwrap();
    assert_eq!(info.signatures.len(), 2);
    let check = verify_signatures(
        &cavs_object::sign::bundle_message(&checksum),
        &info.signatures,
        &ring,
    );
    assert_eq!(check.accepted.len(), 2);
    assert!(check.is_trusted());
}

/// A signature says who published the bundle. It does not make a damaged one
/// acceptable.
#[test]
fn a_signature_does_not_rescue_an_altered_bundle() {
    use cavs_object::{append_signature, bundle_content_checksum, sign_bundle_checksum};
    use ed25519_dalek::SigningKey;

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("repo.cavsbundle");
    let (store, root) = build(&dir.path().join("objects"), 4);
    create_bundle(
        &store,
        &[(ObjectKind::Commit, root)],
        &out,
        &BundleOptions::default(),
    )
    .unwrap();
    let checksum = bundle_content_checksum(&out, &BundleLimits::default()).unwrap();
    let key = SigningKey::from_bytes(&[5u8; 32]);
    append_signature(
        &out,
        sign_bundle_checksum(&key, &checksum),
        &BundleLimits::default(),
    )
    .unwrap();

    let mut raw = std::fs::read(&out).unwrap();
    let middle = raw.len() / 2;
    raw[middle] ^= 0x01;
    std::fs::write(&out, &raw).unwrap();

    assert!(verify_bundle(&out, &BundleLimits::default()).is_err());
}

#[test]
fn one_key_cannot_sign_the_same_bundle_twice() {
    use cavs_object::{append_signature, bundle_content_checksum, sign_bundle_checksum};
    use ed25519_dalek::SigningKey;

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("repo.cavsbundle");
    let (store, root) = build(&dir.path().join("objects"), 2);
    create_bundle(
        &store,
        &[(ObjectKind::Commit, root)],
        &out,
        &BundleOptions::default(),
    )
    .unwrap();
    let checksum = bundle_content_checksum(&out, &BundleLimits::default()).unwrap();
    let key = SigningKey::from_bytes(&[6u8; 32]);
    let signature = sign_bundle_checksum(&key, &checksum);
    append_signature(&out, signature.clone(), &BundleLimits::default()).unwrap();
    assert!(append_signature(&out, signature, &BundleLimits::default()).is_err());
}
