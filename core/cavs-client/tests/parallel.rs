//! End-to-end test for the v1.4.0 content-addressed parallel chunk fetch:
//! real cavs-server + cavs-client, comparing `--connections N` against the
//! sequential `--connections 1` path. Both must reconstruct byte-identically
//! and cost the same wire egress; only wall time should differ.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

fn bin(name: &str) -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    path.pop();
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

fn spawn_server(cavs_file: &str) -> (ServerGuard, String) {
    let mut child = Command::new(bin("cavs-server"))
        .args([cavs_file, "--listen", "127.0.0.1:0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn cavs-server");
    let stdout = child.stdout.take().unwrap();
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line).unwrap();
    let url = line
        .trim()
        .strip_prefix("listening on ")
        .expect("unexpected server banner")
        .to_string();
    (ServerGuard(child), url)
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

fn parse_inline_bytes(output: &str) -> f64 {
    let line = output
        .lines()
        .find(|l| l.starts_with("egress"))
        .unwrap_or_else(|| panic!("no egress line in:\n{output}"));
    let rest = line.split(':').nth(1).unwrap().trim();
    let mut parts = rest.split_whitespace();
    let value: f64 = parts.next().unwrap().parse().unwrap();
    let unit = parts.next().unwrap();
    let mult = match unit {
        "B" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        other => panic!("unexpected unit {other}"),
    };
    value * mult
}

/// A parallel cold fetch and a sequential cold fetch of the same asset must
/// produce byte-identical output and (because both fetch the same immutable
/// missing set, compression-preserving) the same wire egress within rounding.
#[test]
fn parallel_and_sequential_fetch_agree() {
    let dir = tempfile::tempdir().unwrap();
    // Compressible-ish payload so per-chunk zstd actually triggers, proving
    // the stored-bytes negotiation path decompresses+verifies correctly.
    let mut payload = pseudo_random(1_500_000, 91);
    for w in payload.chunks_mut(4096) {
        // Zero out half of each window: gives zstd something to compress.
        for b in w.iter_mut().take(2048) {
            *b = 0;
        }
    }
    let src = dir.path().join("payload.bin");
    std::fs::write(&src, &payload).unwrap();
    let cavs = dir.path().join("payload.cavs");
    let (ok, out) = run(
        "cavs",
        &[
            "pack",
            "--raw",
            src.to_str().unwrap(),
            "--zstd-level",
            "9",
            "-o",
            cavs.to_str().unwrap(),
        ],
    );
    assert!(ok, "pack failed:\n{out}");

    let (_guard, url) = spawn_server(cavs.to_str().unwrap());

    // Sequential (one connection).
    let seq_out = dir.path().join("seq");
    let seq_cache = dir.path().join("seq-cache");
    let (ok, seq) = run(
        "cavs-client",
        &[
            "fetch",
            &url,
            "payload",
            "-o",
            seq_out.to_str().unwrap(),
            "--cache",
            seq_cache.to_str().unwrap(),
            "--connections",
            "1",
        ],
    );
    assert!(ok, "sequential fetch failed:\n{seq}");
    assert_eq!(std::fs::read(seq_out.join("payload.bin")).unwrap(), payload);

    // Parallel (eight connections), fresh cache.
    let par_out = dir.path().join("par");
    let par_cache = dir.path().join("par-cache");
    let (ok, par) = run(
        "cavs-client",
        &[
            "fetch",
            &url,
            "payload",
            "-o",
            par_out.to_str().unwrap(),
            "--cache",
            par_cache.to_str().unwrap(),
            "--connections",
            "8",
        ],
    );
    assert!(ok, "parallel fetch failed:\n{par}");
    assert_eq!(std::fs::read(par_out.join("payload.bin")).unwrap(), payload);

    // Same immutable missing set, same compression -> same wire egress.
    let seq_bytes = parse_inline_bytes(&seq);
    let par_bytes = parse_inline_bytes(&par);
    assert!(
        (seq_bytes - par_bytes).abs() <= seq_bytes * 0.02 + 4096.0,
        "wire egress differs: seq {seq_bytes} vs par {par_bytes}"
    );
    // The compressible payload must have compressed on the wire.
    assert!(
        seq_bytes < payload.len() as f64,
        "expected zstd wire savings, got {seq_bytes} for {} raw",
        payload.len()
    );

    // A warm parallel fetch (reuse the parallel cache) downloads nothing.
    let warm_out = dir.path().join("warm");
    let (ok, warm) = run(
        "cavs-client",
        &[
            "fetch",
            &url,
            "payload",
            "-o",
            warm_out.to_str().unwrap(),
            "--cache",
            par_cache.to_str().unwrap(),
            "--connections",
            "8",
        ],
    );
    assert!(ok, "warm parallel fetch failed:\n{warm}");
    assert_eq!(
        parse_inline_bytes(&warm),
        0.0,
        "warm fetch must be 0 wire:\n{warm}"
    );
    assert_eq!(
        std::fs::read(warm_out.join("payload.bin")).unwrap(),
        payload
    );
}

/// `--connections 0` forces the legacy session/batch path; it must still
/// reconstruct byte-identically (the compatibility fallback).
#[test]
fn legacy_session_path_still_works() {
    let dir = tempfile::tempdir().unwrap();
    let payload = pseudo_random(800_000, 12);
    let src = dir.path().join("b.bin");
    std::fs::write(&src, &payload).unwrap();
    let cavs = dir.path().join("b.cavs");
    let (ok, out) = run(
        "cavs",
        &[
            "pack",
            "--raw",
            src.to_str().unwrap(),
            "-o",
            cavs.to_str().unwrap(),
        ],
    );
    assert!(ok, "pack failed:\n{out}");
    let (_guard, url) = spawn_server(cavs.to_str().unwrap());

    let out_dir = dir.path().join("legacy");
    let cache = dir.path().join("legacy-cache");
    let (ok, log) = run(
        "cavs-client",
        &[
            "fetch",
            &url,
            "b",
            "-o",
            out_dir.to_str().unwrap(),
            "--cache",
            cache.to_str().unwrap(),
            "--connections",
            "0",
        ],
    );
    assert!(ok, "legacy fetch failed:\n{log}");
    assert_eq!(std::fs::read(out_dir.join("b.bin")).unwrap(), payload);
}
