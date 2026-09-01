//! Unit-level test of the embeddable engine against a hand-built static
//! tree (a `Dir` source): one raw file split into three chunks — one stored
//! zstd-compressed, two raw — laid into a minimal `.cavspack`, with a
//! matching `manifest.json` and `chunk-map.json`. Verifies byte-identical
//! reconstruction, the reuse path on a second run, and cancellation.

use cavs_fetch::{fetch_static, FetchError, FetchOptions, StaticSource};
use cavs_hash::{hash_chunk, to_hex};
use std::sync::atomic::AtomicBool;

const PACK_HEADER: &[u8] = b"CAVSPK1\0\x01\x00\x00\x00\x00\x00\x00\x00";

fn build_tree(dir: &std::path::Path) -> Vec<u8> {
    // Three chunks; the middle one is very compressible.
    let c0 = vec![7u8; 40_000];
    let c1 = vec![0u8; 50_000]; // compresses hugely
    let c2 = {
        let mut v = vec![0u8; 30_000];
        let mut s = 123u32;
        for b in v.iter_mut() {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            *b = (s >> 24) as u8;
        }
        v
    };
    let chunks = [c0.clone(), c1.clone(), c2.clone()];

    // Lay the (possibly compressed) chunks into a pack after the 16B header.
    let mut pack = PACK_HEADER.to_vec();
    let mut entries = Vec::new();
    for c in &chunks {
        let stored = zstd::bulk::compress(c, 9).unwrap();
        let (bytes, flags): (Vec<u8>, u32) = if stored.len() < c.len() {
            (stored, 1)
        } else {
            (c.clone(), 0)
        };
        let offset_abs = pack.len() as u64;
        entries.push(serde_json::json!({
            "hash": to_hex(&hash_chunk(c)),
            "len_raw": c.len() as u32,
            "len_stored": bytes.len() as u32,
            "flags": flags,
            "pack": "chunks/packs/00/pack.cavspack",
            "pack_offset_abs": offset_abs,
        }));
        pack.extend_from_slice(&bytes);
    }

    std::fs::create_dir_all(dir.join("chunks/packs/00")).unwrap();
    std::fs::write(dir.join("chunks/packs/00/pack.cavspack"), &pack).unwrap();

    // Manifest: one raw track whose single segment holds the three chunks.
    let chunk_ref =
        |c: &[u8]| serde_json::json!({ "hash": to_hex(&hash_chunk(c)), "len": c.len() });
    let mut full = Vec::new();
    full.extend_from_slice(&c0);
    full.extend_from_slice(&c1);
    full.extend_from_slice(&c2);
    let sha = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(&full);
        h.finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    let manifest = serde_json::json!({
        "asset": "app",
        "asset_uuid": "00000000000000000000000000000000",
        "tracks": [{
            "track_id": 0, "kind": "data", "codec": "raw",
            "name": "payload.bin", "timescale": 0, "init_chunks": []
        }],
        "segments": [{
            "segment_id": 0, "track_id": 0, "pts_start": 0, "duration": 0,
            "random_access": true,
            "chunks": [chunk_ref(&c0), chunk_ref(&c1), chunk_ref(&c2)]
        }],
        "dict": [],
        "chunk_table": [to_hex(&hash_chunk(&c0)), to_hex(&hash_chunk(&c1)), to_hex(&hash_chunk(&c2))],
        "merkle_root": "",
        "signature": null,
        "signer_pubkey": null,
        "meta": [["payload", "raw"], ["sha256:payload.bin", sha]],
    });
    std::fs::create_dir_all(dir.join("assets/app")).unwrap();
    std::fs::write(
        dir.join("assets/app/manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join("assets/app/chunk-map.json"),
        serde_json::to_vec_pretty(&serde_json::json!({ "asset": "app", "chunks": entries }))
            .unwrap(),
    )
    .unwrap();

    full
}

#[test]
fn fetch_static_reconstructs_and_reuses() {
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("dist");
    let full = build_tree(&tree);

    let source = StaticSource::new(tree.to_str().unwrap());
    let out = dir.path().join("install");
    let cache = dir.path().join("cache");

    // Cold install: downloads all three chunks, verifies, reconstructs.
    let opts = FetchOptions {
        connections: 4,
        ..Default::default()
    };
    let stats = fetch_static(&source, "app", &out, &cache, &opts).unwrap();
    assert_eq!(stats.fetched, 3);
    assert_eq!(stats.reused, 0);
    assert!(
        stats.wire_bytes < full.len() as u64,
        "zstd wire savings expected"
    );
    assert_eq!(std::fs::read(out.join("payload.bin")).unwrap(), full);

    // Warm install into a fresh output but the same cache: 0 downloads.
    let out2 = dir.path().join("install2");
    let stats2 = fetch_static(&source, "app", &out2, &cache, &opts).unwrap();
    assert_eq!(stats2.fetched, 0);
    assert_eq!(stats2.reused, 3);
    assert_eq!(std::fs::read(out2.join("payload.bin")).unwrap(), full);
}

#[test]
fn cancellation_stops_the_fetch() {
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("dist");
    build_tree(&tree);
    let source = StaticSource::new(tree.to_str().unwrap());
    let out = dir.path().join("install");
    let cache = dir.path().join("cache");

    // Pre-set the cancel flag: the fetch must abort before completing.
    let cancel = AtomicBool::new(true);
    let opts = FetchOptions {
        connections: 1,
        cancel: Some(&cancel),
        ..Default::default()
    };
    let err = fetch_static(&source, "app", &out, &cache, &opts).unwrap_err();
    assert!(matches!(err, FetchError::Cancelled), "got {err}");
}
