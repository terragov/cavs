//! End-to-end tests driving the `cavs` binary.
//!
//! The raw-mode tests always run. The video-mode test runs only when ffmpeg
//! is available on PATH (it is skipped otherwise, not failed).

use std::path::{Path, PathBuf};
use std::process::Command;

fn cavs_bin() -> PathBuf {
    // target/debug/cavs next to the test binary's directory.
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("cavs");
    path
}

fn run(args: &[&str]) -> (bool, String) {
    let out = Command::new(cavs_bin())
        .args(args)
        .output()
        .expect("failed to run cavs binary");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
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

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn raw_pack_unpack_roundtrip_and_dedup() {
    let dir = tempfile::tempdir().unwrap();

    // file_b shares a large prefix region with file_a (shifted): CDC dedup food.
    let file_a = pseudo_random(2_000_000, 42);
    let mut file_b = b"HEADER-INSERTED-".to_vec();
    file_b.extend_from_slice(&file_a);
    let a_path = dir.path().join("asset_a.bin");
    let b_path = dir.path().join("asset_b.bin");
    std::fs::write(&a_path, &file_a).unwrap();
    std::fs::write(&b_path, &file_b).unwrap();

    let cavs = dir.path().join("assets.cavs");
    let (ok, out) = run(&[
        "pack",
        "--raw",
        a_path.to_str().unwrap(),
        b_path.to_str().unwrap(),
        "-o",
        cavs.to_str().unwrap(),
    ]);
    assert!(ok, "pack failed:\n{out}");
    assert!(out.contains("savings"), "missing stats output:\n{out}");

    let (ok, out) = run(&["verify", cavs.to_str().unwrap()]);
    assert!(ok, "verify failed:\n{out}");
    assert!(out.contains("OK:"), "unexpected verify output:\n{out}");

    let (ok, out) = run(&["info", cavs.to_str().unwrap()]);
    assert!(ok, "info failed:\n{out}");
    assert!(out.contains("CAVS-1.0"), "unexpected info output:\n{out}");

    let outdir = dir.path().join("restored");
    let (ok, out) = run(&[
        "unpack",
        cavs.to_str().unwrap(),
        "-o",
        outdir.to_str().unwrap(),
    ]);
    assert!(ok, "unpack failed:\n{out}");

    // Byte-for-byte reconstruction.
    assert_eq!(std::fs::read(outdir.join("asset_a.bin")).unwrap(), file_a);
    assert_eq!(std::fs::read(outdir.join("asset_b.bin")).unwrap(), file_b);

    // Shifted duplicate content must have deduped: the .cavs must be much
    // smaller than the ~4 MB of logical input.
    let cavs_size = std::fs::metadata(&cavs).unwrap().len();
    let logical: u64 = (file_a.len() + file_b.len()) as u64;
    assert!(
        cavs_size < logical * 3 / 4,
        "expected CDC dedup to save >25%: cavs={cavs_size} logical={logical}"
    );
}

#[test]
fn identical_file_dedupes_completely() {
    let dir = tempfile::tempdir().unwrap();
    let data = pseudo_random(1_500_000, 7);
    let p1 = dir.path().join("one.bin");
    let p2 = dir.path().join("two.bin");
    std::fs::write(&p1, &data).unwrap();
    std::fs::write(&p2, &data).unwrap();

    let cavs = dir.path().join("dup.cavs");
    let (ok, out) = run(&[
        "pack",
        "--raw",
        "--no-compress",
        p1.to_str().unwrap(),
        p2.to_str().unwrap(),
        "-o",
        cavs.to_str().unwrap(),
    ]);
    assert!(ok, "pack failed:\n{out}");

    // Two identical files: stored payload ~= one copy.
    let cavs_size = std::fs::metadata(&cavs).unwrap().len();
    assert!(
        (cavs_size as usize) < data.len() + data.len() / 4,
        "second copy should be ~free: cavs={cavs_size}, one copy={}",
        data.len()
    );
}

#[test]
fn video_pack_unpack_verify() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();

    // Generate a small synthetic test clip (video+audio).
    let clip = dir.path().join("clip.mp4");
    let ok = Command::new("ffmpeg")
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=6:size=640x360:rate=30",
        ])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=6"])
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-c:a",
            "aac",
            "-shortest",
        ])
        .arg(&clip)
        .status()
        .unwrap()
        .success();
    assert!(ok, "could not generate test clip");

    let cavs = dir.path().join("clip.cavs");
    let (ok, out) = run(&[
        "pack",
        clip.to_str().unwrap(),
        "-o",
        cavs.to_str().unwrap(),
        "--segment-time",
        "2",
    ]);
    assert!(ok, "video pack failed:\n{out}");

    let (ok, out) = run(&["verify", cavs.to_str().unwrap()]);
    assert!(ok, "verify failed:\n{out}");

    let outdir = dir.path().join("restored");
    let (ok, out) = run(&[
        "unpack",
        cavs.to_str().unwrap(),
        "-o",
        outdir.to_str().unwrap(),
    ]);
    assert!(ok, "unpack failed:\n{out}");

    // HLS layout present and the combined mp4 decodes cleanly end-to-end.
    assert!(outdir.join("clip/init.mp4").exists());
    assert!(outdir.join("clip/media.m3u8").exists());
    assert!(outdir.join("clip/seg_00000.m4s").exists());
    let combined = outdir.join("clip.mp4");
    assert!(combined.exists());

    let decode_ok = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-xerror", "-i"])
        .arg(&combined)
        .args(["-f", "null", "-"])
        .status()
        .unwrap()
        .success();
    assert!(decode_ok, "reconstructed mp4 must decode without errors");

    check_probe_duration(&combined);
}

fn check_probe_duration(path: &Path) {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .unwrap();
    let dur: f64 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0.0);
    assert!(
        (dur - 6.0).abs() < 1.0,
        "expected ~6s clip after reconstruction, got {dur}s"
    );
}

#[test]
fn manifest_export_and_bench() {
    let dir = tempfile::tempdir().unwrap();
    let payload = pseudo_random(3_000_000, 21);
    let pck = dir.path().join("build.pck");
    std::fs::write(&pck, &payload).unwrap();
    let cavs = dir.path().join("build.cavs");
    let (ok, out) = run(&[
        "pack",
        "--raw",
        pck.to_str().unwrap(),
        "-o",
        cavs.to_str().unwrap(),
    ]);
    assert!(ok, "pack failed:\n{out}");

    // Export: readable JSON v1 with the expected fields.
    let exported = dir.path().join("manifest.debug.json");
    let (ok, out) = run(&[
        "manifest",
        "export",
        cavs.to_str().unwrap(),
        "--out",
        exported.to_str().unwrap(),
    ]);
    assert!(ok, "manifest export failed:\n{out}");
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&exported).unwrap()).unwrap();
    assert_eq!(json["asset"], "build");
    assert!(json["chunk_table"].as_array().unwrap().len() > 1);

    // Bench: reports both formats and the v0.3.0 acceptance threshold
    // (binary v2 at least 50% smaller than JSON v1).
    let report = dir.path().join("bench.json");
    let (ok, out) = run(&[
        "manifest",
        "bench",
        cavs.to_str().unwrap(),
        "--json",
        report.to_str().unwrap(),
    ]);
    assert!(ok, "manifest bench failed:\n{out}");
    assert!(out.contains("json-v1"), "bench must report v1:\n{out}");
    assert!(out.contains("binary-v2"), "bench must report v2:\n{out}");
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    let v1 = report["json_v1"]["wire_bytes"].as_u64().unwrap();
    let v2 = report["binary_v2"]["wire_bytes"].as_u64().unwrap();
    assert!(
        v2 * 2 <= v1,
        "binary v2 ({v2}) must be at least 50% smaller than JSON v1 ({v1})"
    );
}

#[test]
fn packfile_store_add_stat_verify_export() {
    let dir = tempfile::tempdir().unwrap();
    let payload = pseudo_random(2_500_000, 33);
    let pck = dir.path().join("content.pck");
    std::fs::write(&pck, &payload).unwrap();
    let cavs = dir.path().join("content.cavs");
    let (ok, out) = run(&[
        "pack",
        "--raw",
        pck.to_str().unwrap(),
        "-o",
        cavs.to_str().unwrap(),
    ]);
    assert!(ok, "pack failed:\n{out}");

    let store = dir.path().join("store");
    let (ok, out) = run(&[
        "store",
        store.to_str().unwrap(),
        "add",
        "app",
        cavs.to_str().unwrap(),
        "--storage",
        "packfiles",
    ]);
    assert!(ok, "store add failed:\n{out}");

    // stat reports the packfile occupancy line.
    let (ok, out) = run(&["store", store.to_str().unwrap(), "stat"]);
    assert!(ok, "stat failed:\n{out}");
    assert!(out.contains("packfiles"), "missing pack stats:\n{out}");

    // verify passes on a healthy store.
    let (ok, out) = run(&["store", store.to_str().unwrap(), "verify"]);
    assert!(ok, "verify failed:\n{out}");
    assert!(out.contains("OK"), "unexpected verify output:\n{out}");

    // export produces the deterministic immutable object tree.
    let dist = dir.path().join("dist");
    let (ok, out) = run(&[
        "store",
        store.to_str().unwrap(),
        "export",
        "--out",
        dist.to_str().unwrap(),
    ]);
    assert!(ok, "export failed:\n{out}");
    assert!(dist.join("chunks/packs").is_dir());
    assert!(dist.join("assets/app/record.json").is_file());

    // Corrupt one byte of a pack: verify must fail.
    fn find_pack(dir: &Path) -> Option<PathBuf> {
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = find_pack(&path) {
                    return Some(found);
                }
            } else if path.extension().is_some_and(|e| e == "cavspack") {
                return Some(path);
            }
        }
        None
    }
    let pack = find_pack(&store.join("packs")).expect("no pack written");
    let mut bytes = std::fs::read(&pack).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xff;
    std::fs::write(&pack, &bytes).unwrap();
    let (ok, out) = run(&["store", store.to_str().unwrap(), "verify"]);
    assert!(!ok, "verify must fail on a corrupted pack:\n{out}");

    // Adding to the same store with the other layout is rejected.
    let (ok, out) = run(&[
        "store",
        store.to_str().unwrap(),
        "add",
        "game2",
        cavs.to_str().unwrap(),
        "--storage",
        "loose",
    ]);
    assert!(!ok, "layout mismatch must be rejected:\n{out}");
}

#[test]
fn corruption_matrix_passes_on_valid_container() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let payload = pseudo_random(1_500_000, 71);
    std::fs::write(d.join("m.bin"), &payload).unwrap();
    let (ok, out) = run(&[
        "pack",
        "--raw",
        d.join("m.bin").to_str().unwrap(),
        "--bootstrap",
        "-o",
        d.join("m.cavs").to_str().unwrap(),
    ]);
    assert!(ok, "pack failed:\n{out}");

    let report = d.join("corrupt-report.json");
    let (ok, out) = run(&[
        "test",
        "corrupt",
        d.join("m.cavs").to_str().unwrap(),
        "--out",
        report.to_str().unwrap(),
    ]);
    assert!(ok, "corruption matrix failed:\n{out}");
    assert!(
        out.contains("all corrupted inputs were rejected cleanly"),
        "{out}"
    );
    // Every family of targets ran, including bootstrap and packfiles.
    for target in [
        "container_magic",
        "manifest_magic",
        "varint",
        "bootstrap_sidecar",
        "packfile_header",
        "pack_index_bytes",
    ] {
        assert!(out.contains(target), "matrix missing {target}:\n{out}");
    }
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    let tests = json["tests"].as_array().unwrap();
    assert!(tests.len() >= 15, "only {} matrix rows", tests.len());
    assert!(tests.iter().all(|t| t["result"] == "pass"));
}

#[test]
fn doctor_reports_ok_and_detects_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let payload = pseudo_random(800_000, 13);
    std::fs::write(d.join("g.bin"), &payload).unwrap();
    let (ok, out) = run(&[
        "pack",
        "--raw",
        d.join("g.bin").to_str().unwrap(),
        "--bootstrap",
        "-o",
        d.join("g.cavs").to_str().unwrap(),
    ]);
    assert!(ok, "pack failed:\n{out}");

    // Healthy container + a packfile store holding it.
    let (ok, out) = run(&[
        "store",
        d.join("store").to_str().unwrap(),
        "add",
        "g",
        d.join("g.cavs").to_str().unwrap(),
        "--storage",
        "packfiles",
    ]);
    assert!(ok, "store add failed:\n{out}");
    let (ok, out) = run(&[
        "doctor",
        d.join("g.cavs").to_str().unwrap(),
        "--store",
        d.join("store").to_str().unwrap(),
    ]);
    assert!(ok, "doctor failed on healthy deployment:\n{out}");
    assert!(out.contains("Result: OK"), "{out}");
    assert!(out.contains("Bootstrap: OK"), "{out}");

    // Corrupt one chunk in the container: doctor must fail with the code.
    let mut bytes = std::fs::read(d.join("g.cavs")).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xff;
    std::fs::write(d.join("g.cavs"), &bytes).unwrap();
    let (ok, out) = run(&["doctor", d.join("g.cavs").to_str().unwrap()]);
    assert!(!ok, "doctor must fail on corruption:\n{out}");
    assert!(out.contains("Result: FAIL"), "{out}");
    assert!(out.contains("CAVS-E-"), "{out}");
}

#[test]
fn bench_gen_and_suite_produce_reports() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let dataset = d.join("dataset");
    let (ok, out) = run(&[
        "bench",
        "gen",
        "--out",
        dataset.to_str().unwrap(),
        "--size",
        "2MiB",
    ]);
    assert!(ok, "bench gen failed:\n{out}");
    for f in [
        "v1.bin",
        "v2-small.bin",
        "v2-medium.bin",
        "v2-large.bin",
        "v2-shifted.bin",
        "v2-reordered.bin",
        "dataset.json",
    ] {
        assert!(dataset.join(f).is_file(), "missing {f}");
    }
    // Determinism: the same seed regenerates identical bytes.
    let first = std::fs::read(dataset.join("v2-small.bin")).unwrap();
    let dataset2 = d.join("dataset2");
    let (ok, _) = run(&[
        "bench",
        "gen",
        "--out",
        dataset2.to_str().unwrap(),
        "--size",
        "2MiB",
    ]);
    assert!(ok);
    assert_eq!(first, std::fs::read(dataset2.join("v2-small.bin")).unwrap());

    let results = d.join("results");
    let (ok, out) = run(&[
        "bench",
        "suite",
        "--dataset",
        dataset.to_str().unwrap(),
        "--out",
        results.to_str().unwrap(),
    ]);
    assert!(ok, "bench suite failed:\n{out}");
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(results.join("summary.json")).unwrap())
            .unwrap();
    let versions = json["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 6);
    // The small update must move far fewer bytes than the large one.
    let egress = |name: &str| {
        versions.iter().find(|v| v["name"] == name).unwrap()["update_egress_bytes"]
            .as_u64()
            .unwrap()
    };
    assert!(egress("v2-small.bin") < egress("v2-large.bin"));
    assert!(json["packstore"]["packfiles"].as_u64().unwrap() >= 1);
    assert!(results.join("summary.md").is_file());
}
