//! End-to-end tests for the `cavs certify` family (v1.0.0).
//!
//! Every test drives the real binary and asserts on the documented
//! contract: exit codes, report files, JSON schemas and the mandatory
//! byte-identical rule.

use std::path::{Path, PathBuf};
use std::process::Command;

fn cavs_bin() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop(); // deps/
    path.pop(); // debug/
    path.push("cavs");
    path
}

fn run(args: &[&str]) -> (i32, String) {
    let out = Command::new(cavs_bin())
        .args(args)
        .output()
        .expect("failed to run cavs binary");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text)
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

/// A small directory build pair: modified, unchanged, deleted and added
/// files, with one file large enough (>64 KiB blocks) to exercise the
/// block diff.
fn make_dir_pair(root: &Path) -> (PathBuf, PathBuf) {
    let old = root.join("Build_v1");
    let new = root.join("Build_v2");
    std::fs::create_dir_all(old.join("assets")).unwrap();
    std::fs::create_dir_all(new.join("assets")).unwrap();

    let big = pseudo_random(400_000, 1);
    std::fs::write(old.join("payload.bin"), &big).unwrap();
    let mut big2 = big.clone();
    big2[200_000..200_064].copy_from_slice(&[0xAB; 64]);
    std::fs::write(new.join("payload.bin"), &big2).unwrap();

    let unchanged = pseudo_random(150_000, 2);
    std::fs::write(old.join("assets/level.dat"), &unchanged).unwrap();
    std::fs::write(new.join("assets/level.dat"), &unchanged).unwrap();

    std::fs::write(old.join("assets/removed.dat"), pseudo_random(80_000, 3)).unwrap();
    std::fs::write(new.join("assets/added.dat"), pseudo_random(80_000, 4)).unwrap();
    (old, new)
}

/// A minimal well-formed Godot PCK (same layout as
/// `analyze_packs::godot_pck::synth`, format 2).
fn synth_pck(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut header = Vec::new();
    header.extend_from_slice(&0x4350_4447u32.to_le_bytes()); // "GDPC"
    header.extend_from_slice(&2u32.to_le_bytes());
    for v in [4u32, 2, 0] {
        header.extend_from_slice(&v.to_le_bytes());
    }
    header.extend_from_slice(&0u32.to_le_bytes()); // flags
    header.extend_from_slice(&0u64.to_le_bytes()); // file_base
    header.extend_from_slice(&[0u8; 16 * 4]);
    header.extend_from_slice(&(files.len() as u32).to_le_bytes());
    let mut dir_size = 0usize;
    for (path, _) in files {
        dir_size += 4 + path.len() + 8 + 8 + 16 + 4;
    }
    let payload_start = header.len() + dir_size;
    let mut dir = Vec::new();
    let mut payloads = Vec::new();
    let mut offset = payload_start as u64;
    for (path, bytes) in files {
        dir.extend_from_slice(&(path.len() as u32).to_le_bytes());
        dir.extend_from_slice(path.as_bytes());
        dir.extend_from_slice(&offset.to_le_bytes());
        dir.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        dir.extend_from_slice(&[0u8; 16]);
        dir.extend_from_slice(&0u32.to_le_bytes());
        payloads.extend_from_slice(bytes);
        offset += bytes.len() as u64;
    }
    header.extend_from_slice(&dir);
    header.extend_from_slice(&payloads);
    header
}

fn read_json(path: &Path) -> serde_json::Value {
    let bytes =
        std::fs::read(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("{} is not JSON: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// Full orchestrator
// ---------------------------------------------------------------------------

#[test]
fn certify_directory_pair_release_profile_passes() {
    let dir = tempfile::tempdir().unwrap();
    let (old, new) = make_dir_pair(dir.path());
    let out = dir.path().join("certification");

    let (code, text) = run(&[
        "certify",
        "--old",
        old.to_str().unwrap(),
        "--new",
        new.to_str().unwrap(),
        "--profile",
        "release",
        "--routes",
        "estimate",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "certify failed:\n{text}");
    assert!(text.contains("Result: PASS"), "missing verdict:\n{text}");

    // The documented report bundle exists.
    for f in [
        "summary.md",
        "summary.json",
        "integrity.md",
        "integrity.json",
        "routes.md",
        "routes.json",
        "steampipe-style.md",
        "pack-analysis.md",
        "io-estimate.md",
        "dependencies.json",
        "environment.json",
        "commands.sh",
        "artifacts/old.cavssig",
        "artifacts/new.cavssig",
        "artifacts/update.cavsplan",
        "artifacts/hashes.json",
    ] {
        assert!(out.join(f).exists(), "missing report file {f}");
    }

    let summary = read_json(&out.join("summary.json"));
    assert_eq!(summary["schema"], "cavs-certify-summary/1");
    assert_eq!(summary["result"], "pass");
    assert_eq!(summary["byte_identical"], true);
    assert_eq!(summary["exit_code"], 0);
    let integrity = read_json(&out.join("integrity.json"));
    assert_eq!(integrity["byte_identical"], true);
}

#[test]
fn certify_strict_profile_runs_corruption_and_repro() {
    let dir = tempfile::tempdir().unwrap();
    let (old, new) = make_dir_pair(dir.path());
    let out = dir.path().join("certification");

    let (code, text) = run(&[
        "certify",
        "--old",
        old.to_str().unwrap(),
        "--new",
        new.to_str().unwrap(),
        "--profile",
        "strict",
        "--routes",
        "estimate",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "strict certify failed:\n{text}");
    let integrity = std::fs::read_to_string(out.join("integrity.md")).unwrap();
    for check in [
        "corrupt signature rejected | PASS",
        "corrupt plan rejected | PASS",
        "corrupted old input fails safely | PASS",
        "no-op reapply | PASS",
    ] {
        assert!(
            integrity.contains(check),
            "missing '{check}' in:\n{integrity}"
        );
    }
    // Strict exports a repro bundle by default.
    assert!(out.join("repro.tar.zst").exists(), "missing repro bundle");
}

#[test]
fn certify_invalid_inputs_exit_4_and_write_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("certification");
    let (code, _) = run(&[
        "certify",
        "--old",
        dir.path().join("missing").to_str().unwrap(),
        "--new",
        dir.path().join("missing2").to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code, 4, "invalid input must exit 4");
    assert!(!out.exists(), "invalid input must not create the out dir");

    let (code, _) = run(&["certify"]);
    assert_eq!(code, 4, "missing inputs must exit 4");

    let (old, new) = make_dir_pair(dir.path());
    let (code, _) = run(&[
        "certify",
        "--old",
        old.to_str().unwrap(),
        "--new",
        new.to_str().unwrap(),
        "--profile",
        "nope",
    ]);
    assert_eq!(code, 4, "unknown profile must exit 4");
}

// ---------------------------------------------------------------------------
// certify integrity
// ---------------------------------------------------------------------------

#[test]
fn certify_integrity_passes_and_corrupt_plan_fails() {
    let dir = tempfile::tempdir().unwrap();
    let (old, new) = make_dir_pair(dir.path());
    let out = dir.path().join("int");

    let (code, text) = run(&[
        "certify",
        "integrity",
        "--old",
        old.to_str().unwrap(),
        "--new",
        new.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "integrity failed:\n{text}");
    let report = read_json(&out.join("integrity.json"));
    assert_eq!(report["byte_identical"], true);

    // A bit-flipped plan handed to certification must FAIL it (exit 1).
    let plan = out.join("artifacts/update.cavsplan");
    let mut bytes = std::fs::read(&plan).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    let bad = dir.path().join("bad.cavsplan");
    std::fs::write(&bad, bytes).unwrap();
    let (code, text) = run(&[
        "certify",
        "integrity",
        "--plan",
        bad.to_str().unwrap(),
        "--out",
        dir.path().join("int2").to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "corrupt plan must fail certification:\n{text}");
}

#[test]
fn certify_integrity_artifact_pair() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("v1.bin");
    let new = dir.path().join("v2.bin");
    let base = pseudo_random(500_000, 9);
    std::fs::write(&old, &base).unwrap();
    let mut changed = base.clone();
    changed[250_000..250_128].copy_from_slice(&[0x5A; 128]);
    std::fs::write(&new, &changed).unwrap();

    let (code, text) = run(&[
        "certify",
        "integrity",
        "--old",
        old.to_str().unwrap(),
        "--new",
        new.to_str().unwrap(),
        "--out",
        dir.path().join("int").to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "artifact integrity failed:\n{text}");
}

// ---------------------------------------------------------------------------
// certify routes
// ---------------------------------------------------------------------------

#[test]
fn certify_routes_covers_default_states_and_cold_install_never_needs_previous() {
    let dir = tempfile::tempdir().unwrap();
    let (old, new) = make_dir_pair(dir.path());
    let out = dir.path().join("routes");

    let (code, text) = run(&[
        "certify",
        "routes",
        "--old",
        old.to_str().unwrap(),
        "--new",
        new.to_str().unwrap(),
        "--routes",
        "estimate",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "routes failed:\n{text}");
    let report = read_json(&out.join("routes.json"));
    assert_eq!(report["schema"], "cavs-certify-routes/1");
    let states = report["states"].as_array().unwrap();
    let names: Vec<&str> = states
        .iter()
        .map(|s| s["state"].as_str().unwrap())
        .collect();
    for expected in [
        "cold-install",
        "cold-cache-previous",
        "warm-cache",
        "exact-previous-version",
        "low-ram",
        "slow-hdd",
        "limited-disk",
    ] {
        assert!(names.contains(&expected), "missing state {expected}");
    }
    let cold = states
        .iter()
        .find(|s| s["state"] == "cold-install")
        .unwrap();
    let chosen = cold["chosen"].as_str().unwrap();
    assert!(
        !chosen.contains("plan") && !chosen.contains("hybrid") && !chosen.contains("no-op"),
        "cold install chose a previous-install route: {chosen}"
    );
    // Scores and weights are part of the frozen JSON contract.
    assert!(report["weights"].is_object(), "missing policy weights");
    assert!(
        states[0]["routes"][0]["score"].is_number(),
        "missing route scores"
    );
}

#[test]
fn certify_routes_measured_matrix_verifies_outputs() {
    let dir = tempfile::tempdir().unwrap();
    // Artifact pair keeps the measured matrix fast.
    let old = dir.path().join("v1.bin");
    let new = dir.path().join("v2.bin");
    let base = pseudo_random(300_000, 21);
    std::fs::write(&old, &base).unwrap();
    let mut changed = base.clone();
    changed[100_000..100_064].copy_from_slice(&[0x77; 64]);
    std::fs::write(&new, &changed).unwrap();
    let out = dir.path().join("routes");

    let (code, text) = run(&[
        "certify",
        "routes",
        "--old",
        old.to_str().unwrap(),
        "--new",
        new.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "measured routes failed:\n{text}");
    let report = read_json(&out.join("routes.json"));
    let measured = report["measured"]["routes"].as_array().unwrap();
    assert!(!measured.is_empty(), "no measured routes");
    for r in measured {
        assert_ne!(
            r["output_ok"], false,
            "route {} produced a non-identical output",
            r["route"]
        );
    }
    assert!(out.join("artifacts/route-results.json").exists());
}

// ---------------------------------------------------------------------------
// certify regressions
// ---------------------------------------------------------------------------

fn write_metrics(path: &Path, network: f64, apply_ms: f64, byte_identical: bool) {
    std::fs::write(
        path,
        serde_json::json!({
            "schema": "cavs-certify-baseline/1",
            "byte_identical": byte_identical,
            "metrics": { "network_bytes": network, "apply_ms": apply_ms }
        })
        .to_string(),
    )
    .unwrap();
}

#[test]
fn certify_regressions_thresholds_exceptions_and_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let baseline = dir.path().join("baseline.json");
    let current = dir.path().join("current.json");
    write_metrics(&baseline, 1000.0, 100.0, true);

    // Within thresholds: pass.
    write_metrics(&current, 1020.0, 105.0, true);
    let (code, text) = run(&[
        "certify",
        "regressions",
        "--current",
        current.to_str().unwrap(),
        "--baseline",
        baseline.to_str().unwrap(),
        "--out",
        dir.path().join("r1").to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "within-threshold run must pass:\n{text}");

    // +50% network: fail.
    write_metrics(&current, 1500.0, 100.0, true);
    let (code, _) = run(&[
        "certify",
        "regressions",
        "--current",
        current.to_str().unwrap(),
        "--baseline",
        baseline.to_str().unwrap(),
        "--out",
        dir.path().join("r2").to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "network regression must fail");

    // Same regression with an explicit exception: warning (exit 2).
    let (code, _) = run(&[
        "certify",
        "regressions",
        "--current",
        current.to_str().unwrap(),
        "--baseline",
        baseline.to_str().unwrap(),
        "--allow-regression",
        "network_bytes=new compression default, accepted for v1.0.0",
        "--out",
        dir.path().join("r3").to_str().unwrap(),
    ]);
    assert_eq!(code, 2, "excepted regression must exit 2");

    // --fail-on-warning turns that into a failure.
    let (code, _) = run(&[
        "certify",
        "regressions",
        "--current",
        current.to_str().unwrap(),
        "--baseline",
        baseline.to_str().unwrap(),
        "--allow-regression",
        "network_bytes=accepted",
        "--fail-on-warning",
        "--out",
        dir.path().join("r4").to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "--fail-on-warning must exit 1");

    // Losing byte-identical status always fails, thresholds aside.
    write_metrics(&current, 1000.0, 100.0, false);
    let (code, _) = run(&[
        "certify",
        "regressions",
        "--current",
        current.to_str().unwrap(),
        "--baseline",
        baseline.to_str().unwrap(),
        "--out",
        dir.path().join("r5").to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "byte-identical regression must fail");
}

#[test]
fn certify_full_run_against_saved_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let (old, new) = make_dir_pair(dir.path());
    let baseline = dir.path().join("baseline.json");

    let (code, text) = run(&[
        "certify",
        "--old",
        old.to_str().unwrap(),
        "--new",
        new.to_str().unwrap(),
        "--profile",
        "quick",
        "--save-baseline",
        baseline.to_str().unwrap(),
        "--out",
        dir.path().join("c1").to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "baseline run failed:\n{text}");
    assert!(baseline.exists());

    // Same pair vs its own baseline: no regression possible.
    let (code, text) = run(&[
        "certify",
        "--old",
        old.to_str().unwrap(),
        "--new",
        new.to_str().unwrap(),
        "--profile",
        "ci",
        "--routes",
        "estimate",
        "--baseline",
        baseline.to_str().unwrap(),
        "--json-out",
        dir.path().join("certification.json").to_str().unwrap(),
        "--out",
        dir.path().join("c2").to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "ci run vs own baseline failed:\n{text}");
    let summary = read_json(&dir.path().join("certification.json"));
    assert_eq!(summary["result"], "pass");
    let has_regression_section = summary["sections"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["name"] == "Regression");
    assert!(
        has_regression_section,
        "ci profile must run the regression guard"
    );
}

// ---------------------------------------------------------------------------
// certify godot
// ---------------------------------------------------------------------------

#[test]
fn certify_godot_pck_pair() {
    let dir = tempfile::tempdir().unwrap();
    let hero = pseudo_random(300_000, 31);
    let level = pseudo_random(200_000, 32);
    let mut hero2 = hero.clone();
    hero2[150_000..150_064].copy_from_slice(&[0xEE; 64]);
    let old = dir.path().join("old.pck");
    let new = dir.path().join("new.pck");
    std::fs::write(
        &old,
        synth_pck(&[
            ("res://textures/hero.png", &hero),
            ("res://levels/l1.scn", &level),
        ]),
    )
    .unwrap();
    std::fs::write(
        &new,
        synth_pck(&[
            ("res://textures/hero.png", &hero2),
            ("res://levels/l1.scn", &level),
        ]),
    )
    .unwrap();
    let out = dir.path().join("godot");

    let plugin_dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../game-engine-plugins/godot-plugin/addons");
    let mut args = vec![
        "certify".to_string(),
        "godot".to_string(),
        "--old-pck".into(),
        old.display().to_string(),
        "--new-pck".into(),
        new.display().to_string(),
        "--out".into(),
        out.display().to_string(),
    ];
    if plugin_dir.is_dir() {
        args.push("--plugin-dir".into());
        args.push(plugin_dir.display().to_string());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, text) = run(&arg_refs);
    assert_eq!(code, 0, "godot certify failed:\n{text}");
    let report = read_json(&out.join("godot.json"));
    assert_eq!(report["byte_identical"], true);
    assert!(out.join("godot.md").exists());
    assert!(out.join("godot-pck-analysis.md").exists());
    if plugin_dir.is_dir() {
        let md = std::fs::read_to_string(out.join("godot.md")).unwrap();
        assert!(
            md.contains("plugin API surface | PASS"),
            "plugin API check did not pass:\n{md}"
        );
    }
}

#[test]
fn certify_autodetects_godot_mode_for_pck_pairs() {
    let dir = tempfile::tempdir().unwrap();
    let data = pseudo_random(200_000, 41);
    let mut data2 = data.clone();
    data2[100_000..100_032].copy_from_slice(&[0x11; 32]);
    let old = dir.path().join("old.pck");
    let new = dir.path().join("new.pck");
    std::fs::write(&old, synth_pck(&[("res://a.bin", &data)])).unwrap();
    std::fs::write(&new, synth_pck(&[("res://a.bin", &data2)])).unwrap();
    let out = dir.path().join("cert");

    let (code, text) = run(&[
        "certify",
        "--old",
        old.to_str().unwrap(),
        "--new",
        new.to_str().unwrap(),
        "--profile",
        "release",
        "--routes",
        "estimate",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "pck certify failed:\n{text}");
    let summary = read_json(&out.join("summary.json"));
    assert_eq!(summary["mode"], "godot-pck");
    assert!(
        out.join("godot.md").exists(),
        "godot section must run for .pck pairs"
    );
}

// ---------------------------------------------------------------------------
// certify workspace
// ---------------------------------------------------------------------------

#[test]
fn certify_workspace_builds_branches_and_install_plans() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("cavs-workspace");
    let src_base_v1 = dir.path().join("src/base-v1");
    let src_base_v2 = dir.path().join("src/base-v2");
    let src_win = dir.path().join("src/windows");
    let src_es = dir.path().join("src/lang-es");
    for d in [&src_base_v1, &src_base_v2, &src_win, &src_es] {
        std::fs::create_dir_all(d).unwrap();
    }
    let content = pseudo_random(200_000, 51);
    std::fs::write(src_base_v1.join("payload.bin"), &content).unwrap();
    let mut content2 = content.clone();
    content2[100_000..100_032].copy_from_slice(&[0x22; 32]);
    std::fs::write(src_base_v2.join("payload.bin"), &content2).unwrap();
    std::fs::write(src_win.join("launcher.exe"), pseudo_random(50_000, 52)).unwrap();
    std::fs::write(src_es.join("es.txt"), b"hola").unwrap();

    let ws_s = ws.to_str().unwrap();
    assert_eq!(run(&["workspace", "init", ws_s, "--app", "my-app"]).0, 0);
    assert_eq!(run(&["depot", "add", "base", "--workspace", ws_s]).0, 0);
    assert_eq!(
        run(&[
            "depot",
            "add",
            "windows",
            "--platform",
            "windows",
            "--workspace",
            ws_s
        ])
        .0,
        0
    );
    assert_eq!(
        run(&[
            "depot",
            "add",
            "lang-es",
            "--language",
            "es",
            "--optional",
            "--workspace",
            ws_s
        ])
        .0,
        0
    );
    assert_eq!(run(&["branch", "add", "beta", "--workspace", ws_s]).0, 0);
    for (label, base) in [("build_1001", &src_base_v1), ("build_1002", &src_base_v2)] {
        let (code, text) = run(&[
            "build",
            "create",
            "--workspace",
            ws_s,
            "--branch",
            "beta",
            "--depot",
            &format!("base={}", base.display()),
            "--depot",
            &format!("windows={}", src_win.display()),
            "--depot",
            &format!("lang-es={}", src_es.display()),
            "--label",
            label,
        ]);
        assert_eq!(code, 0, "build create failed:\n{text}");
    }

    let out = dir.path().join("cert-ws");
    let (code, text) = run(&[
        "certify",
        "workspace",
        "--workspace",
        ws_s,
        "--from",
        "build_1001",
        "--to",
        "build_1002",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "workspace certify failed:\n{text}");
    let md = std::fs::read_to_string(out.join("workspace.md")).unwrap();
    for check in [
        "metadata parse | PASS",
        "branches valid | PASS",
        "branch promote preview | PASS",
        "rollback preview | PASS",
        "depot sharing | PASS",
        "per-depot update cost | PASS",
    ] {
        assert!(md.contains(check), "missing '{check}' in:\n{md}");
    }
    assert!(
        md.contains("install-plan"),
        "no install-plan states in:\n{md}"
    );
    assert!(out.join("workspace.json").exists());
    assert!(out.join("depot-sharing.md").exists());
}

#[test]
fn certify_workspace_missing_build_fails_clearly() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let ws_s = ws.to_str().unwrap();
    assert_eq!(run(&["workspace", "init", ws_s, "--app", "g"]).0, 0);
    let (code, text) = run(&[
        "certify",
        "workspace",
        "--workspace",
        ws_s,
        "--from",
        "nope_1",
        "--to",
        "nope_2",
        "--out",
        dir.path().join("out").to_str().unwrap(),
    ]);
    assert_eq!(code, 1, "missing builds must fail certification:\n{text}");
}

// ---------------------------------------------------------------------------
// certify export-repro
// ---------------------------------------------------------------------------

#[test]
fn export_repro_is_deterministic_and_excludes_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let (old, new) = make_dir_pair(dir.path());
    let out = dir.path().join("certification");
    let (code, text) = run(&[
        "certify",
        "--old",
        old.to_str().unwrap(),
        "--new",
        new.to_str().unwrap(),
        "--profile",
        "quick",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "quick certify failed:\n{text}");

    let b1 = dir.path().join("repro1.tar.zst");
    let b2 = dir.path().join("repro2.tar.zst");
    for b in [&b1, &b2] {
        let (code, text) = run(&[
            "certify",
            "export-repro",
            "--certification",
            out.to_str().unwrap(),
            "--out",
            b.to_str().unwrap(),
        ]);
        assert_eq!(code, 0, "export-repro failed:\n{text}");
    }
    let bytes1 = std::fs::read(&b1).unwrap();
    let bytes2 = std::fs::read(&b2).unwrap();
    assert_eq!(bytes1, bytes2, "repro bundle is not deterministic");

    // The default bundle must not embed the input builds: it stays far
    // smaller than the inputs and its tar lists no inputs/files/ entries.
    let tar = zstd::stream::decode_all(bytes1.as_slice()).unwrap();
    let tar_text = String::from_utf8_lossy(&tar);
    assert!(tar_text.contains("repro/README.md"), "bundle misses README");
    assert!(
        tar_text.contains("repro/commands.sh"),
        "bundle misses commands"
    );
    assert!(tar_text.contains("repro/outputs/report-hashes.json"));
    assert!(
        !tar_text.contains("inputs/files/"),
        "default bundle must not include input files"
    );
}
