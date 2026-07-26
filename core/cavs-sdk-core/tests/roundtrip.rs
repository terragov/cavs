//! End-to-end conformance for the SDK operation engine over generated
//! fixtures: analyze → pack → createPlan → applyPlan (byte-identical) →
//! verifyInstall → preview → benchmark → estimateSavings.

use cavs_sdk_core::execute_envelope;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// Build a deterministic "old" and "new" directory build sharing most of
/// their bytes so the diff routes have real reuse to find.
fn make_builds(root: &Path) -> (PathBuf, PathBuf) {
    let old = root.join("Build_v1");
    let new = root.join("Build_v2");
    fs::create_dir_all(old.join("data")).unwrap();
    fs::create_dir_all(new.join("data")).unwrap();

    // A large, mostly-shared asset: 512 KiB of a repeating pattern.
    let base: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
    fs::write(old.join("data/asset.bin"), &base).unwrap();
    let mut changed = base.clone();
    // Flip a small window in the middle; the rest should be reused.
    for b in changed.iter_mut().skip(300_000).take(4096) {
        *b ^= 0xFF;
    }
    fs::write(new.join("data/asset.bin"), &changed).unwrap();

    // An unchanged text file (pure reuse) and a brand-new file.
    fs::write(old.join("readme.txt"), b"cavs sdk fixture\n").unwrap();
    fs::write(new.join("readme.txt"), b"cavs sdk fixture\n").unwrap();
    fs::write(new.join("data/new_only.bin"), vec![7u8; 64 * 1024]).unwrap();

    (old, new)
}

fn ok_data(operation: &str, data: Value) -> Value {
    let request = json!({ "schemaVersion": "1.0", "data": data }).to_string();
    let out = execute_envelope(operation, &request, None, None);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], true, "{operation} failed: {out}");
    v["data"].clone()
}

#[test]
fn full_pipeline_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let (old, new) = make_builds(tmp.path());

    // 1. analyze
    let analyze = ok_data("analyze", json!({ "oldPath": old, "newPath": new }));
    assert!(analyze["summary"]["newSizeBytes"].as_u64().unwrap() > 0);
    assert!(analyze["summary"]["estimatedUpdateBytes"].as_u64().unwrap() > 0);

    // 2. packDirectory
    let out_cavs = tmp.path().join("build_v2.cavs");
    let pack = ok_data(
        "packDirectory",
        json!({ "inputDir": new, "outputCavs": out_cavs }),
    );
    assert!(out_cavs.is_file());
    assert!(pack["filesPacked"].as_u64().unwrap() >= 3);

    // 3. createPlan (portable) old → new
    let plan_path = tmp.path().join("update.cavsplan");
    let plan = ok_data(
        "createPlan",
        json!({ "oldPath": old, "newPath": new, "outputPlan": plan_path }),
    );
    assert!(plan_path.is_file());
    assert!(
        plan["reusedBytes"].as_u64().unwrap() > 0,
        "diff found no reuse"
    );
    let plan_bytes = plan["planBytes"].as_u64().unwrap();

    // 4. applyPlan → byte-identical reconstruction of `new`
    let out_dir = tmp.path().join("Build_v2_out");
    let apply = ok_data(
        "applyPlan",
        json!({ "oldPath": old, "planPath": plan_path, "outputPath": out_dir }),
    );
    assert_eq!(apply["verified"], true);
    assert_files_equal(&new, &out_dir);

    // 5. verifyInstall via a manifest of the packed container — build a
    //    signature of the reconstructed output and verify it.
    let sig_path = tmp.path().join("new.cavssig");
    let sig = cavs_signature::CavsSignature::sign_dir(&new, 64 * 1024, "new").unwrap();
    fs::write(&sig_path, sig.encode()).unwrap();
    let verify = ok_data(
        "verifyInstall",
        json!({ "target": out_dir, "signature": sig_path }),
    );
    assert_eq!(verify["verified"], true, "verify: {verify}");
    assert!(verify["filesChecked"].as_u64().unwrap() >= 3);

    // verify should fail on a corrupted output
    fs::write(out_dir.join("readme.txt"), b"tampered\n").unwrap();
    let bad = ok_data(
        "verifyInstall",
        json!({ "target": out_dir, "signature": sig_path }),
    );
    assert_eq!(bad["verified"], false);
    assert!(!bad["mismatches"]["modified"].as_array().unwrap().is_empty());

    // 6. previewUpdate — the recommended route is the cheapest by bytes,
    //    and the cavsPlan route matches the plan we actually produced.
    let preview = ok_data("previewUpdate", json!({ "oldPath": old, "newPath": new }));
    let routes = preview["routes"].as_array().unwrap();
    let cheapest = routes
        .iter()
        .min_by_key(|r| r["networkBytes"].as_u64().unwrap())
        .unwrap();
    assert_eq!(preview["recommendedRoute"], cheapest["name"]);
    let cavs_route = routes.iter().find(|r| r["name"] == "cavsPlan").unwrap();
    assert_eq!(cavs_route["networkBytes"].as_u64().unwrap(), plan_bytes);

    // 7. benchmark
    let bench = ok_data(
        "benchmark",
        json!({ "oldPath": old, "newPath": new, "measureApply": false }),
    );
    assert_eq!(bench["recommendedRoute"], "cavsPlan");
    assert_eq!(bench["routes"].as_array().unwrap().len(), 4);

    // 8. estimateSavings
    let savings = ok_data(
        "estimateSavings",
        json!({
            "pricePerGb": 0.08,
            "monthlyDownloads": 500000,
            "averageFullDownloadBytes": 65011712,
            "averageCavsDownloadBytes": 2631921
        }),
    );
    assert!(savings["savingsPercent"].as_f64().unwrap() > 90.0);
    assert!(
        savings["estimatedMonthlySavings"].as_f64().unwrap()
            > savings["cavsMonthlyCost"].as_f64().unwrap()
    );
}

#[test]
fn preview_route_filter() {
    let tmp = tempfile::tempdir().unwrap();
    let (old, new) = make_builds(tmp.path());
    let preview = ok_data(
        "previewUpdate",
        json!({ "oldPath": old, "newPath": new, "routes": ["cavsPlan", "fullRaw"] }),
    );
    assert_eq!(preview["routes"].as_array().unwrap().len(), 2);
}

#[test]
fn pack_respects_ignore_patterns() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("proj");
    fs::create_dir_all(dir.join("logs")).unwrap();
    fs::write(dir.join("keep.bin"), vec![1u8; 1024]).unwrap();
    fs::write(dir.join("logs/skip.log"), vec![2u8; 1024]).unwrap();
    let out = tmp.path().join("proj.cavs");
    let pack = ok_data(
        "packDirectory",
        json!({ "inputDir": dir, "outputCavs": out, "ignore": ["*.log"] }),
    );
    assert_eq!(pack["filesPacked"].as_u64().unwrap(), 1);
    assert!(pack["entriesIgnored"].as_u64().unwrap() >= 1);
}

/// `fetchStatic` is registered, reaches the fetch engine (not the
/// unknown-operation path), and reconstructs a build from a hand-built
/// static tree through the JSON envelope — the operation a launcher or app
/// embeds to self-update.
#[test]
fn fetch_static_op_reconstructs_from_static_tree() {
    use cavs_hash::{hash_chunk, to_hex};

    // It must appear in the advertised capabilities.
    let caps: Value = serde_json::from_str(&cavs_sdk_core::capabilities_json()).unwrap();
    assert!(caps["features"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f == "fetchStatic"));

    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("dist");

    // One raw file = two chunks laid uncompressed into a minimal pack.
    let c0 = vec![9u8; 20_000];
    let mut c1 = vec![0u8; 20_000];
    let mut s = 7u32;
    for b in c1.iter_mut() {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        *b = (s >> 24) as u8;
    }
    let mut pack = b"CAVSPK1\0\x01\x00\x00\x00\x00\x00\x00\x00".to_vec();
    let mut entries = Vec::new();
    for c in [&c0, &c1] {
        let off = pack.len() as u64;
        entries.push(json!({
            "hash": to_hex(&hash_chunk(c)), "len_raw": c.len() as u32,
            "len_stored": c.len() as u32, "flags": 0,
            "pack": "chunks/packs/00/p.cavspack", "pack_offset_abs": off,
        }));
        pack.extend_from_slice(c);
    }
    fs::create_dir_all(tree.join("chunks/packs/00")).unwrap();
    fs::write(tree.join("chunks/packs/00/p.cavspack"), &pack).unwrap();
    fs::create_dir_all(tree.join("assets/app")).unwrap();
    let manifest = json!({
        "asset": "app", "asset_uuid": "0".repeat(32),
        "tracks": [{ "track_id": 0, "kind": "data", "codec": "raw",
                     "name": "payload.bin", "timescale": 0, "init_chunks": [] }],
        "segments": [{ "segment_id": 0, "track_id": 0, "pts_start": 0,
                       "duration": 0, "random_access": true,
                       "chunks": [ {"hash": to_hex(&hash_chunk(&c0)), "len": c0.len()},
                                   {"hash": to_hex(&hash_chunk(&c1)), "len": c1.len()} ] }],
        "dict": [], "chunk_table": [], "merkle_root": "",
        "signature": null, "signer_pubkey": null, "meta": [["payload","raw"]],
    });
    fs::write(
        tree.join("assets/app/manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    fs::write(
        tree.join("assets/app/chunk-map.json"),
        serde_json::to_vec(&json!({"asset":"app","chunks":entries})).unwrap(),
    )
    .unwrap();

    let out = tmp.path().join("install");
    let cache = tmp.path().join("cache");
    let data = ok_data(
        "fetchStatic",
        json!({
            "base": tree, "asset": "app",
            "outputDir": out, "cacheDir": cache, "connections": 2
        }),
    );
    assert_eq!(data["chunksFetched"].as_u64().unwrap(), 2);
    let mut full = c0.clone();
    full.extend_from_slice(&c1);
    assert_eq!(fs::read(out.join("payload.bin")).unwrap(), full);
}

fn assert_files_equal(a: &Path, b: &Path) {
    let mut checked = 0;
    for entry in walkdir(a) {
        let rel = entry.strip_prefix(a).unwrap();
        let other = b.join(rel);
        if entry.is_file() {
            assert!(other.is_file(), "missing {}", rel.display());
            assert_eq!(
                fs::read(&entry).unwrap(),
                fs::read(&other).unwrap(),
                "content differs for {}",
                rel.display()
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "no files compared");
}

fn walkdir(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path.clone());
            }
            out.push(path);
        }
    }
    out
}
