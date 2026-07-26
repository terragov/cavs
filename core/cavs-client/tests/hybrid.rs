//! End-to-end tests of v0.6.0 hybrid reconstruction: real cavs +
//! cavs-server + cavs-client binaries over HTTP.
//!
//! Covers: previous-artifact reuse with an empty cache, corruption
//! fallback, no-op detection, plan dumping, and the directory/container
//! preview (pack-dir → fetch → per-file no-op on update).

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

fn pack_raw(d: &Path, input: &str, output: &str, prev: Option<&str>) {
    let mut args = vec![
        "pack".to_string(),
        "--raw".to_string(),
        d.join(input).display().to_string(),
        "--profile".to_string(),
        "auto".to_string(),
        "--bootstrap".to_string(),
        "-o".to_string(),
        d.join(output).display().to_string(),
    ];
    if let Some(p) = prev {
        args.push("--prev".to_string());
        args.push(d.join(p).display().to_string());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (ok, out) = run("cavs", &arg_refs);
    assert!(ok, "pack {input} failed:\n{out}");
}

/// The headline v0.6.0 scenario: a client with an EMPTY cache but the old
/// version installed updates by copying verified ranges from the old file;
/// the network cost is the changed chunks, not the full build.
#[test]
fn empty_cache_with_previous_artifact_updates_cheaply() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();

    let v1 = pseudo_random(6 * 1024 * 1024, 7);
    let mut v2 = v1.clone();
    let at = 2 * 1024 * 1024;
    v2[at..at + 300 * 1024].copy_from_slice(&pseudo_random(300 * 1024, 8));
    std::fs::write(d.join("game_v1.pck"), &v1).unwrap();
    std::fs::write(d.join("game_v2.pck"), &v2).unwrap();
    pack_raw(d, "game_v1.pck", "game_v1.cavs", None);
    pack_raw(d, "game_v2.pck", "game_v2.cavs", Some("game_v1.cavs"));

    let (_guard, url) = spawn_server(&[&d.join("game_v1.cavs"), &d.join("game_v2.cavs")]);

    // Empty cache + previous artifact + plan dump.
    let (ok, out) = run(
        "cavs-client",
        &[
            "fetch",
            &url,
            "game_v2",
            "-o",
            d.join("out").to_str().unwrap(),
            "--cache",
            d.join("cache").to_str().unwrap(),
            "--previous-artifact",
            d.join("game_v1.pck").to_str().unwrap(),
            "--dump-plan",
            d.join("plan.json").to_str().unwrap(),
            "--stats-json",
            d.join("s.json").to_str().unwrap(),
        ],
    );
    assert!(ok, "hybrid fetch failed:\n{out}");
    assert_eq!(std::fs::read(d.join("out/game_v2.pck")).unwrap(), v2);

    let s = stats(&d.join("s.json"));
    // AC4: bytes sourced from the previous artifact show up in stats.
    let prev_bytes = s["sources"]["previous_artifact_bytes"].as_u64().unwrap();
    assert!(
        prev_bytes > v2.len() as u64 / 2,
        "too little previous reuse: {s}"
    );
    // AC1: the wire cost must be a fraction of the full build even though
    // the chunk cache started empty.
    let wire = s["sources"]["network_bytes"].as_u64().unwrap();
    assert!(
        wire < v2.len() as u64 / 4,
        "network too expensive with previous artifact: {wire} of {}",
        v2.len()
    );
    // Plan dump is valid JSON with hybrid ops.
    let plans: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(d.join("plan.json")).unwrap()).unwrap();
    let ops = plans[0]["stats"]["copy_previous_range_ops"]
        .as_u64()
        .unwrap();
    assert!(ops > 0, "plan has no previous-range ops: {plans}");
    let coalesced = plans[0]["stats"]["coalesced_ops"].as_u64().unwrap();
    assert!(coalesced > 0, "no coalescing happened: {plans}");
}

/// AC2: a corrupt previous artifact demotes to network/cache per range and
/// the output stays byte-identical; the mismatch is reported.
#[test]
fn corrupt_previous_artifact_falls_back_to_network() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();

    let v1 = pseudo_random(4 * 1024 * 1024, 21);
    let mut v2 = v1.clone();
    v2[1024 * 1024..1024 * 1024 + 100 * 1024].copy_from_slice(&pseudo_random(100 * 1024, 22));
    std::fs::write(d.join("game_v1.pck"), &v1).unwrap();
    std::fs::write(d.join("game_v2.pck"), &v2).unwrap();
    pack_raw(d, "game_v1.pck", "game_v1.cavs", None);
    pack_raw(d, "game_v2.pck", "game_v2.cavs", Some("game_v1.cavs"));

    // Corrupt a byte of the previous install (bit rot).
    let mut corrupt = v1.clone();
    corrupt[3 * 1024 * 1024] ^= 0xff;
    std::fs::write(d.join("game_v1.pck"), &corrupt).unwrap();

    let (_guard, url) = spawn_server(&[&d.join("game_v2.cavs")]);
    let (ok, out) = run(
        "cavs-client",
        &[
            "fetch",
            &url,
            "game_v2",
            "-o",
            d.join("out").to_str().unwrap(),
            "--cache",
            d.join("cache").to_str().unwrap(),
            "--previous-artifact",
            d.join("game_v1.pck").to_str().unwrap(),
            "--stats-json",
            d.join("s.json").to_str().unwrap(),
        ],
    );
    assert!(ok, "fetch with corrupt previous failed:\n{out}");
    assert_eq!(std::fs::read(d.join("out/game_v2.pck")).unwrap(), v2);
    // The corruption is confined: the poisoned chunk was simply not indexed
    // (or was demoted with the stable error code) — either way the output
    // verified and most of the file still came from the previous artifact.
    let s = stats(&d.join("s.json"));
    assert!(s["sources"]["previous_artifact_bytes"].as_u64().unwrap() > 0);
}

/// No-op levels 1 and 2: an output that already matches costs nothing; a
/// previous artifact that IS the new version installs locally.
#[test]
fn no_op_detection_skips_work() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let v1 = pseudo_random(2 * 1024 * 1024, 31);
    std::fs::write(d.join("game_v1.pck"), &v1).unwrap();
    pack_raw(d, "game_v1.pck", "game_v1.cavs", None);
    let (_guard, url) = spawn_server(&[&d.join("game_v1.cavs")]);

    // First fetch: real work.
    let (ok, out) = run(
        "cavs-client",
        &[
            "fetch",
            &url,
            "game_v1",
            "-o",
            d.join("out").to_str().unwrap(),
            "--cache",
            d.join("cache").to_str().unwrap(),
        ],
    );
    assert!(ok, "first fetch failed:\n{out}");

    // Second fetch: level-1 no-op (output matches), zero wire.
    let (ok, out) = run(
        "cavs-client",
        &[
            "fetch",
            &url,
            "game_v1",
            "-o",
            d.join("out").to_str().unwrap(),
            "--cache",
            d.join("cache-fresh").to_str().unwrap(),
            "--stats-json",
            d.join("s2.json").to_str().unwrap(),
        ],
    );
    assert!(ok, "no-op fetch failed:\n{out}");
    let s2 = stats(&d.join("s2.json"));
    assert_eq!(s2["delivery_mode"], "no-op", "stats: {s2}");
    assert_eq!(s2["inline_bytes"].as_u64().unwrap(), 0);
    assert_eq!(s2["no_op"], true);

    // Level-2: fresh output dir, previous artifact == target → local copy.
    let (ok, out) = run(
        "cavs-client",
        &[
            "fetch",
            &url,
            "game_v1",
            "-o",
            d.join("out2").to_str().unwrap(),
            "--cache",
            d.join("cache-fresh2").to_str().unwrap(),
            "--previous-artifact",
            d.join("game_v1.pck").to_str().unwrap(),
            "--stats-json",
            d.join("s3.json").to_str().unwrap(),
        ],
    );
    assert!(ok, "previous-copy fetch failed:\n{out}");
    let s3 = stats(&d.join("s3.json"));
    assert_eq!(s3["delivery_mode"], "previous-copy", "stats: {s3}");
    assert_eq!(s3["inline_bytes"].as_u64().unwrap(), 0);
    assert_eq!(std::fs::read(d.join("out2/game_v1.pck")).unwrap(), v1);

    // --force-reconstruct bypasses both.
    let (ok, out) = run(
        "cavs-client",
        &[
            "fetch",
            &url,
            "game_v1",
            "-o",
            d.join("out").to_str().unwrap(),
            "--cache",
            d.join("cache").to_str().unwrap(),
            "--force-reconstruct",
            "--stats-json",
            d.join("s4.json").to_str().unwrap(),
        ],
    );
    assert!(ok, "forced fetch failed:\n{out}");
    let s4 = stats(&d.join("s4.json"));
    assert_ne!(s4["delivery_mode"], "no-op");
}

/// --no-hybrid must reproduce v0.5 behaviour: the previous artifact is
/// ignored and everything flows through cache/network.
#[test]
fn no_hybrid_flag_disables_previous_reuse() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let v1 = pseudo_random(2 * 1024 * 1024, 61);
    std::fs::write(d.join("a.pck"), &v1).unwrap();
    pack_raw(d, "a.pck", "a.cavs", None);
    let (_guard, url) = spawn_server(&[&d.join("a.cavs")]);
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
            "--previous-artifact",
            d.join("a.pck").to_str().unwrap(),
            "--no-hybrid",
            "--force-reconstruct",
            "--stats-json",
            d.join("s.json").to_str().unwrap(),
        ],
    );
    assert!(ok, "no-hybrid fetch failed:\n{out}");
    let s = stats(&d.join("s.json"));
    // Without hybrid, nothing may come from the previous artifact.
    assert_eq!(
        s["sources"]["previous_artifact_bytes"]
            .as_u64()
            .unwrap_or(0),
        0,
        "stats: {s}"
    );
    assert_eq!(std::fs::read(d.join("out/a.pck")).unwrap(), v1);
}

/// Directory/container preview: pack-dir v1 → install → pack-dir v2 with
/// one changed file → update touches only the changed file; unchanged
/// (possibly modded) files are left alone; prune removes dropped files.
#[test]
fn directory_mode_updates_only_changed_files() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();

    // Build v1: three files, one nested.
    let build1 = d.join("Build_v1");
    std::fs::create_dir_all(build1.join("levels")).unwrap();
    std::fs::write(build1.join("payload.bin"), pseudo_random(700_000, 71)).unwrap();
    std::fs::write(build1.join("levels/l1.dat"), pseudo_random(400_000, 72)).unwrap();
    std::fs::write(build1.join("dropme.txt"), b"obsolete").unwrap();

    // Build v2: payload.bin changes, l1.dat unchanged, dropme.txt removed,
    // new file added.
    let build2 = d.join("Build_v2");
    std::fs::create_dir_all(build2.join("levels")).unwrap();
    let mut payload2 = std::fs::read(build1.join("payload.bin")).unwrap();
    payload2[100_000..100_100].copy_from_slice(&pseudo_random(100, 73));
    std::fs::write(build2.join("payload.bin"), &payload2).unwrap();
    std::fs::copy(build1.join("levels/l1.dat"), build2.join("levels/l1.dat")).unwrap();
    std::fs::write(build2.join("newfile.bin"), pseudo_random(50_000, 74)).unwrap();

    let (ok, out) = run(
        "cavs",
        &[
            "pack-dir",
            build1.to_str().unwrap(),
            "-o",
            d.join("b1.cavs").to_str().unwrap(),
        ],
    );
    assert!(ok, "pack-dir v1 failed:\n{out}");
    let (ok, out) = run(
        "cavs",
        &[
            "pack-dir",
            build2.to_str().unwrap(),
            "-o",
            d.join("b2.cavs").to_str().unwrap(),
        ],
    );
    assert!(ok, "pack-dir v2 failed:\n{out}");

    let (_guard, url) = spawn_server(&[&d.join("b1.cavs"), &d.join("b2.cavs")]);
    let install = d.join("InstalledGame");

    // Install v1.
    let (ok, out) = run(
        "cavs-client",
        &[
            "fetch",
            &url,
            "b1",
            "-o",
            install.to_str().unwrap(),
            "--cache",
            d.join("cache").to_str().unwrap(),
        ],
    );
    assert!(ok, "install v1 failed:\n{out}");
    assert_eq!(
        std::fs::read(install.join("levels/l1.dat")).unwrap(),
        std::fs::read(build1.join("levels/l1.dat")).unwrap()
    );
    assert!(install.join("dropme.txt").is_file());
    assert!(
        !install.join(".cavs-staging").exists(),
        "staging not cleaned"
    );

    // Age l1.dat's mtime so we can prove it is not rewritten.
    let old_mtime = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
    std::fs::File::options()
        .write(true)
        .open(install.join("levels/l1.dat"))
        .unwrap()
        .set_modified(old_mtime)
        .unwrap();

    // Update to v2 with prune.
    let (ok, out) = run(
        "cavs-client",
        &[
            "fetch",
            &url,
            "b2",
            "-o",
            install.to_str().unwrap(),
            "--cache",
            d.join("cache").to_str().unwrap(),
            "--prune",
            "--stats-json",
            d.join("s.json").to_str().unwrap(),
        ],
    );
    assert!(ok, "update v2 failed:\n{out}");
    assert_eq!(
        std::fs::read(install.join("payload.bin")).unwrap(),
        payload2
    );
    assert_eq!(
        std::fs::read(install.join("newfile.bin")).unwrap(),
        std::fs::read(build2.join("newfile.bin")).unwrap()
    );
    // The unchanged file was not touched (same mtime), and stats say so.
    let mtime = std::fs::metadata(install.join("levels/l1.dat"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(mtime, old_mtime, "unchanged file was rewritten");
    let s = stats(&d.join("s.json"));
    assert!(s["no_op_files"].as_u64().unwrap() >= 1, "stats: {s}");
    // Pruned the dropped file.
    assert!(
        !install.join("dropme.txt").exists(),
        "prune did not remove dropme.txt"
    );
    assert!(
        !install.join(".cavs-staging").exists(),
        "staging not cleaned"
    );
}
