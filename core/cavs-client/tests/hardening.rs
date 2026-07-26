//! End-to-end tests of the v0.5.0 hardening features: resume of an
//! interrupted bootstrap download over HTTP Range, the resume journal
//! lifecycle, cache verify/repair/gc, and structured CAVS-E-* error codes.
//! Real cavs + cavs-server + cavs-client binaries over HTTP.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

fn bin(name: &str) -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push(name);
    path
}

fn run(binary: &str, args: &[&str]) -> (bool, String) {
    let out = Command::new(bin(binary))
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {binary}: {e}"));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

struct ServerGuard(Child);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_server(cavs_files: &[&Path]) -> (ServerGuard, String) {
    let mut cmd = Command::new(bin("cavs-server"));
    for f in cavs_files {
        cmd.arg(f);
    }
    let mut child = cmd
        .args(["--listen", "127.0.0.1:0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn cavs-server");
    let stdout = child.stdout.take().unwrap();
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .expect("server did not print its address");
    let url = line
        .trim()
        .strip_prefix("listening on ")
        .expect("unexpected server banner")
        .to_string();
    (ServerGuard(child), url)
}

fn stats(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("stats json missing"))
        .expect("bad stats json")
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

/// Payload with long-range redundancy so the bootstrap route wins for a
/// cold cache (see dual_route.rs).
fn build_like(len: usize, seed: u32) -> Vec<u8> {
    let half = len / 2;
    let mut out = pseudo_random(half, seed);
    let mut echo = out.clone();
    for i in (0..echo.len()).step_by(97) {
        echo[i] ^= 0x5A;
    }
    out.extend_from_slice(&echo);
    out.truncate(len);
    out
}

fn pack_with_bootstrap(d: &Path, name: &str, payload: &[u8]) -> PathBuf {
    let pck = d.join(format!("{name}.pck"));
    let cavs = d.join(format!("{name}.cavs"));
    std::fs::write(&pck, payload).unwrap();
    let (ok, out) = run(
        "cavs",
        &[
            "pack",
            "--raw",
            pck.to_str().unwrap(),
            "--profile",
            "auto",
            "--bootstrap",
            "-o",
            cavs.to_str().unwrap(),
        ],
    );
    assert!(ok, "pack failed:\n{out}");
    assert!(d.join(format!("{name}.cavs.bootstrap.zst")).is_file());
    cavs
}

/// The manifest exactly as the client receives it (binary v2 negotiated),
/// to fabricate a matching resume journal.
fn manifest_blake3(url: &str, asset: &str) -> String {
    use std::io::Read as _;
    let resp = ureq::get(&format!("{url}/api/assets/{asset}/manifest"))
        .set(
            "accept",
            "application/vnd.cavs.manifest-v2, application/json;q=0.5",
        )
        .call()
        .expect("manifest fetch failed");
    let mut bytes = Vec::new();
    resp.into_reader().read_to_end(&mut bytes).unwrap();
    cavs_hash::to_hex(&cavs_hash::hash_chunk(&bytes))
}

/// Fabricate the exact on-disk state an interrupted bootstrap download
/// leaves behind: a truncated `.zst.part` (a byte-exact prefix of the
/// immutable artifact) plus the journal describing it.
fn fabricate_interrupted_bootstrap(
    d: &Path,
    cache: &Path,
    url: &str,
    asset: &str,
    out_dir: &Path,
    keep_bytes: usize,
) -> (u64, PathBuf) {
    let sidecar = std::fs::read(d.join(format!("{asset}.cavs.bootstrap.zst"))).unwrap();
    assert!(keep_bytes < sidecar.len());
    std::fs::create_dir_all(out_dir).unwrap();
    let part = out_dir.join(format!("{asset}.pck.bootstrap.zst.part"));
    std::fs::write(&part, &sidecar[..keep_bytes]).unwrap();

    let full_b3 = cavs_hash::to_hex(&cavs_hash::hash_chunk(&sidecar));
    let journal_dir = cache.join("journal");
    std::fs::create_dir_all(&journal_dir).unwrap();
    let journal = serde_json::json!({
        "asset": asset,
        "server": url,
        "output": out_dir,
        "manifest_blake3": manifest_blake3(url, asset),
        "state": "bootstrap-downloading",
        "bootstrap_part": part,
        "bootstrap_blake3": full_b3,
        "updated_at": 0,
    });
    std::fs::write(
        journal_dir.join(format!("{asset}.resume.json")),
        serde_json::to_vec_pretty(&journal).unwrap(),
    )
    .unwrap();
    (sidecar.len() as u64, part)
}

#[test]
fn interrupted_bootstrap_resumes_with_range() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let payload = build_like(8 * 1024 * 1024, 42);
    let cavs = pack_with_bootstrap(d, "app", &payload);
    let (_guard, url) = spawn_server(&[&cavs]);

    let cache = d.join("cache");
    let out_dir = d.join("out");
    let keep = 512 * 1024;
    let (full_len, part) = fabricate_interrupted_bootstrap(d, &cache, &url, "app", &out_dir, keep);

    // `resume` picks the journal up and continues the download.
    let (ok, out) = run(
        "cavs-client",
        &["resume", "--cache", cache.to_str().unwrap()],
    );
    assert!(ok, "resume failed:\n{out}");
    assert!(
        out.contains("continuing bootstrap download"),
        "no range resume happened:\n{out}"
    );
    assert_eq!(std::fs::read(out_dir.join("app.pck")).unwrap(), payload);
    // Only the missing tail traveled, the partial file is gone, and the
    // journal is cleared.
    assert!(!part.exists());
    assert!(!cache.join("journal/app.resume.json").exists());
    let _ = full_len;

    let (ok, out) = run(
        "cavs-client",
        &["resume", "--cache", cache.to_str().unwrap()],
    );
    assert!(ok);
    assert!(out.contains("nothing to resume"), "{out}");
}

#[test]
fn resumed_download_pays_only_the_missing_tail() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let payload = build_like(8 * 1024 * 1024, 77);
    let cavs = pack_with_bootstrap(d, "tail", &payload);
    let (_guard, url) = spawn_server(&[&cavs]);

    let cache = d.join("cache");
    let out_dir = d.join("out");
    // Keep 75% of the artifact: the resumed fetch may only pay ~25%.
    let sidecar_len = std::fs::metadata(d.join("tail.cavs.bootstrap.zst"))
        .unwrap()
        .len() as usize;
    let keep = sidecar_len * 3 / 4;
    fabricate_interrupted_bootstrap(d, &cache, &url, "tail", &out_dir, keep);

    let (ok, out) = run(
        "cavs-client",
        &[
            "fetch",
            &url,
            "tail",
            "-o",
            out_dir.to_str().unwrap(),
            "--cache",
            cache.to_str().unwrap(),
            "--stats-json",
            d.join("s.json").to_str().unwrap(),
        ],
    );
    assert!(ok, "fetch failed:\n{out}");
    assert!(out.contains("continuing bootstrap download"), "{out}");
    let s = stats(&d.join("s.json"));
    assert_eq!(s["delivery_mode"], "bootstrap");
    let wire = s["inline_bytes"].as_u64().unwrap();
    assert!(
        wire <= (sidecar_len - keep) as u64 + 1024,
        "resume paid {wire} for a {} byte tail",
        sidecar_len - keep
    );
    assert_eq!(std::fs::read(out_dir.join("tail.pck")).unwrap(), payload);
}

#[test]
fn no_resume_discards_partial_state() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let payload = build_like(4 * 1024 * 1024, 9);
    let cavs = pack_with_bootstrap(d, "clean", &payload);
    let (_guard, url) = spawn_server(&[&cavs]);

    let cache = d.join("cache");
    let out_dir = d.join("out");
    let (_, part) = fabricate_interrupted_bootstrap(d, &cache, &url, "clean", &out_dir, 64 * 1024);

    let (ok, out) = run(
        "cavs-client",
        &[
            "fetch",
            &url,
            "clean",
            "-o",
            out_dir.to_str().unwrap(),
            "--cache",
            cache.to_str().unwrap(),
            "--no-resume",
        ],
    );
    assert!(ok, "fetch failed:\n{out}");
    assert!(
        !out.contains("continuing bootstrap download"),
        "--no-resume must not resume:\n{out}"
    );
    assert_eq!(std::fs::read(out_dir.join("clean.pck")).unwrap(), payload);
    assert!(!part.exists());
}

#[test]
fn stale_journal_is_discarded() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let payload = build_like(4 * 1024 * 1024, 33);
    let cavs = pack_with_bootstrap(d, "stale", &payload);
    let (_guard, url) = spawn_server(&[&cavs]);

    let cache = d.join("cache");
    let out_dir = d.join("out");
    let (_, part) = fabricate_interrupted_bootstrap(d, &cache, &url, "stale", &out_dir, 64 * 1024);
    // Sabotage the journal's manifest hash: simulates a republished asset.
    let jpath = cache.join("journal/stale.resume.json");
    let mut j: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&jpath).unwrap()).unwrap();
    j["manifest_blake3"] = serde_json::Value::String("00".repeat(32));
    std::fs::write(&jpath, serde_json::to_vec(&j).unwrap()).unwrap();

    let (ok, out) = run(
        "cavs-client",
        &[
            "fetch",
            &url,
            "stale",
            "-o",
            out_dir.to_str().unwrap(),
            "--cache",
            cache.to_str().unwrap(),
        ],
    );
    assert!(ok, "fetch failed:\n{out}");
    assert!(out.contains("starting clean"), "{out}");
    assert!(!out.contains("continuing bootstrap download"), "{out}");
    assert_eq!(std::fs::read(out_dir.join("stale.pck")).unwrap(), payload);
    assert!(!part.exists());
}

#[test]
fn cache_verify_repair_gc_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    // Incompressible: stays on the chunk route, populating the cache.
    let payload = pseudo_random(3 * 1024 * 1024, 4);
    std::fs::write(d.join("blob.bin"), &payload).unwrap();
    let (ok, out) = run(
        "cavs",
        &[
            "pack",
            "--raw",
            d.join("blob.bin").to_str().unwrap(),
            "-o",
            d.join("blob.cavs").to_str().unwrap(),
        ],
    );
    assert!(ok, "pack failed:\n{out}");
    let (_guard, url) = spawn_server(&[&d.join("blob.cavs")]);

    let cache = d.join("cache");
    let (ok, out) = run(
        "cavs-client",
        &[
            "fetch",
            &url,
            "blob",
            "-o",
            d.join("out").to_str().unwrap(),
            "--cache",
            cache.to_str().unwrap(),
        ],
    );
    assert!(ok, "fetch failed:\n{out}");

    // Corrupt two cached chunks and leave a stray temp file behind.
    let mut corrupted = 0;
    for shard in std::fs::read_dir(&cache).unwrap().flatten() {
        if corrupted >= 2 || !shard.path().is_dir() {
            continue;
        }
        let name = shard.file_name().to_string_lossy().to_string();
        if name.len() != 2 || name == "jo" {
            continue;
        }
        for entry in std::fs::read_dir(shard.path()).unwrap().flatten() {
            if corrupted >= 2 {
                break;
            }
            std::fs::write(entry.path(), b"corrupted payload").unwrap();
            corrupted += 1;
        }
    }
    assert_eq!(corrupted, 2, "expected at least two cached chunks");

    let (ok, out) = run(
        "cavs-client",
        &["cache", "verify", "--cache", cache.to_str().unwrap()],
    );
    assert!(ok, "verify failed:\n{out}");
    assert!(out.contains("CAVS-E-CACHE-CORRUPT-RECOVERABLE"), "{out}");
    assert!(out.contains("2 corrupt entries quarantined"), "{out}");
    assert!(cache.join("quarantine").is_dir());

    // Repair refetches exactly the quarantined chunks.
    let (ok, out) = run(
        "cavs-client",
        &[
            "cache",
            "repair",
            &url,
            "blob",
            "--cache",
            cache.to_str().unwrap(),
        ],
    );
    assert!(ok, "repair failed:\n{out}");
    assert!(out.contains("2 re-fetched"), "{out}");

    // The repaired cache serves a warm, zero-wire, byte-identical fetch.
    let (ok, out) = run(
        "cavs-client",
        &[
            "fetch",
            &url,
            "blob",
            "-o",
            d.join("out2").to_str().unwrap(),
            "--cache",
            cache.to_str().unwrap(),
            "--stats-json",
            d.join("s.json").to_str().unwrap(),
        ],
    );
    assert!(ok, "warm fetch failed:\n{out}");
    let s = stats(&d.join("s.json"));
    assert_eq!(s["inline_bytes"].as_u64().unwrap(), 0, "stats: {s}");
    assert_eq!(std::fs::read(d.join("out2/blob.bin")).unwrap(), payload);

    // GC to a tiny budget evicts, and the next fetch recovers over the wire.
    let (ok, out) = run(
        "cavs-client",
        &[
            "cache",
            "gc",
            "--cache",
            cache.to_str().unwrap(),
            "--max-size",
            "256KiB",
        ],
    );
    assert!(ok, "gc failed:\n{out}");
    assert!(out.contains("evicted"), "{out}");
    let (ok, out) = run(
        "cavs-client",
        &[
            "fetch",
            &url,
            "blob",
            "-o",
            d.join("out3").to_str().unwrap(),
            "--cache",
            cache.to_str().unwrap(),
        ],
    );
    assert!(ok, "post-gc fetch failed:\n{out}");
    assert_eq!(std::fs::read(d.join("out3/blob.bin")).unwrap(), payload);
}

#[test]
fn signature_errors_carry_stable_codes() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let payload = pseudo_random(256 * 1024, 8);
    std::fs::write(d.join("a.bin"), &payload).unwrap();
    let (ok, out) = run(
        "cavs",
        &[
            "pack",
            "--raw",
            d.join("a.bin").to_str().unwrap(),
            "-o",
            d.join("a.cavs").to_str().unwrap(),
        ],
    );
    assert!(ok, "pack failed:\n{out}");
    let (_guard, url) = spawn_server(&[&d.join("a.cavs")]);

    // Requiring a signature on an unsigned asset must fail with the
    // structured signature code, and produce no output artifact.
    let (ok, out) = run(
        "cavs-client",
        &[
            "fetch",
            &url,
            "a",
            "-o",
            d.join("out").to_str().unwrap(),
            "--cache",
            d.join("cache").to_str().unwrap(),
            "--pubkey",
            &"ab".repeat(32),
        ],
    );
    assert!(!ok, "fetch must fail:\n{out}");
    assert!(out.contains("CAVS-E-SIGNATURE-INVALID"), "{out}");
    assert!(!d.join("out/a.bin").exists());
}
