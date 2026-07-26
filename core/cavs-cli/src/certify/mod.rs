//! `cavs certify` — the v1.0.0 release-readiness suite.
//!
//! Orchestrates the existing CAVS tooling (signatures, plans, apply,
//! route planning, SteamPipe-style analysis, pack analysis, I/O
//! estimates, workspace install plans, Godot PCK checks) into one
//! certification run with Markdown + JSON reports, stable exit codes
//! and an optional reproducibility bundle.

pub mod godot;
pub mod integrity;
pub mod regressions;
pub mod repro;
pub mod routes;
pub mod workspace;

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::report::human_bytes;

/// Schema id embedded in `summary.json` (frozen for v1.x; additive only).
pub const SUMMARY_SCHEMA: &str = "cavs-certify-summary/1";
/// Schema id for regression baselines.
pub const BASELINE_SCHEMA: &str = "cavs-certify-baseline/1";

// ---------------------------------------------------------------------------
// Exit codes (documented, frozen for v1.x)
// ---------------------------------------------------------------------------

pub const EXIT_PASS: i32 = 0;
pub const EXIT_FAIL: i32 = 1;
pub const EXIT_WARN: i32 = 2;
pub const EXIT_MISSING_DEP: i32 = 3;
pub const EXIT_INVALID_INPUT: i32 = 4;
pub const EXIT_INTERNAL: i32 = 5;

// ---------------------------------------------------------------------------
// Check results
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CheckResult {
    Pass,
    Skipped,
    Warn,
    Fail,
}

impl CheckResult {
    pub fn label(self) -> &'static str {
        match self {
            CheckResult::Pass => "PASS",
            CheckResult::Skipped => "SKIPPED",
            CheckResult::Warn => "PASS WITH WARNINGS",
            CheckResult::Fail => "FAIL",
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct CheckRow {
    pub name: String,
    pub result: CheckResult,
    pub details: String,
}

impl CheckRow {
    pub fn new(name: &str, result: CheckResult, details: impl Into<String>) -> Self {
        CheckRow {
            name: name.into(),
            result,
            details: details.into(),
        }
    }
}

/// Worst result across a set of rows (Skipped never degrades a section).
pub fn worst(rows: &[CheckRow]) -> CheckResult {
    rows.iter()
        .map(|r| r.result)
        .filter(|r| *r != CheckResult::Skipped)
        .max()
        .unwrap_or(CheckResult::Pass)
}

pub fn rows_markdown(rows: &[CheckRow]) -> String {
    let mut md = String::from("| Check | Result | Details |\n|---|---|---|\n");
    for r in rows {
        md.push_str(&format!(
            "| {} | {} | {} |\n",
            r.name,
            r.result.label(),
            r.details.replace('|', "\\|")
        ));
    }
    md
}

// ---------------------------------------------------------------------------
// Profiles
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    Quick,
    Standard,
    Release,
    Strict,
    Ci,
}

impl Profile {
    pub fn parse(s: &str) -> Result<Profile> {
        Ok(match s {
            "quick" => Profile::Quick,
            "standard" => Profile::Standard,
            "release" => Profile::Release,
            "strict" => Profile::Strict,
            "ci" => Profile::Ci,
            other => bail!(
                "CAVS-E-CERTIFY-PROFILE-INVALID: unknown profile '{other}' \
                 (quick|standard|release|strict|ci)"
            ),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Profile::Quick => "quick",
            Profile::Standard => "standard",
            Profile::Release => "release",
            Profile::Strict => "strict",
            Profile::Ci => "ci",
        }
    }

    /// Measured route matrix (real applies, external tools when installed).
    fn measured_routes(self) -> bool {
        self != Profile::Quick
    }
    /// SteamPipe-style + pack + I/O analysis sections.
    fn analysis(self) -> bool {
        self != Profile::Quick
    }
    /// Regression guard (needs a baseline to actually run).
    fn regressions(self) -> bool {
        matches!(self, Profile::Release | Profile::Strict | Profile::Ci)
    }
    /// Corruption smoke checks inside integrity.
    fn corruption(self) -> bool {
        self == Profile::Strict
    }
    /// Reproducibility bundle exported by default.
    fn repro(self) -> bool {
        self == Profile::Strict
    }
    /// Configured external tools become hard requirements.
    fn tools_required(self) -> bool {
        self == Profile::Strict
    }
}

// ---------------------------------------------------------------------------
// CLI argument structs
// ---------------------------------------------------------------------------

/// Flags of the top-level `cavs certify` orchestrator.
#[derive(clap::Args, Debug)]
pub struct FullArgs {
    /// Old build (file, directory or .pck).
    #[arg(long)]
    pub old: Option<PathBuf>,
    /// New build (same kind as --old).
    #[arg(long)]
    pub new: Option<PathBuf>,
    /// Engine hint: auto|generic|godot (.pck pairs auto-detect godot).
    #[arg(long, default_value = "auto")]
    pub engine: String,
    /// Workspace mode: certify a build transition inside this workspace.
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    /// App id inside the workspace (default: the workspace default).
    #[arg(long)]
    pub app: Option<String>,
    /// Source build id (workspace mode), e.g. build_1001.
    #[arg(long)]
    pub from: Option<String>,
    /// Target build id (workspace mode).
    #[arg(long)]
    pub to: Option<String>,
    /// Path to the butler binary for external comparisons.
    #[arg(long)]
    pub butler_bin: Option<String>,
    /// Path to the xdelta3 binary for pairwise proxies.
    #[arg(long)]
    pub xdelta3_bin: Option<String>,
    /// Path to the bsdiff binary for pairwise proxies.
    #[arg(long)]
    pub bsdiff_bin: Option<String>,
    /// Path to a Godot binary for the optional smoke test.
    #[arg(long)]
    pub godot_bin: Option<String>,
    /// Godot test project for the optional smoke test.
    #[arg(long)]
    pub test_project: Option<PathBuf>,
    /// Regression baseline JSON (from a previous run's routes.json or
    /// `--save-baseline`).
    #[arg(long)]
    pub baseline: Option<PathBuf>,
    /// Max allowed network-bytes regression vs the baseline (e.g. 5%).
    #[arg(long, default_value = "5%")]
    pub max_network_regression: String,
    /// Max allowed apply-time regression vs the baseline (e.g. 10%).
    #[arg(long, default_value = "10%")]
    pub max_apply_regression: String,
    /// Max allowed peak-RAM regression vs the baseline (e.g. 20%).
    #[arg(long, default_value = "20%")]
    pub max_ram_regression: String,
    /// Accept a named regression with an explicit reason: metric=reason
    /// (repeatable).
    #[arg(long)]
    pub allow_regression: Vec<String>,
    /// Save this run's metrics as a baseline for future runs.
    #[arg(long)]
    pub save_baseline: Option<PathBuf>,
    /// Certification profile: quick|standard|release|strict|ci.
    #[arg(long, default_value = "release")]
    pub profile: String,
    /// Client states for route certification (comma-separated; default:
    /// the documented state matrix).
    #[arg(long)]
    pub client_states: Option<String>,
    /// Route-selection policy (see `cavs plan-update --policy`).
    #[arg(long, default_value = "balanced")]
    pub policy: String,
    /// Routes to certify: all (measured matrix) or estimate (planner only).
    #[arg(long, default_value = "all")]
    pub routes: String,
    /// Report directory.
    #[arg(long, default_value = "./certification")]
    pub out: PathBuf,
    /// Also write the summary JSON to this path (CI artifact).
    #[arg(long)]
    pub json_out: Option<PathBuf>,
    /// Exit 1 instead of 2 when warnings are present.
    #[arg(long)]
    pub fail_on_warning: bool,
    /// Export a reproducibility bundle (tar.zst) after certification.
    #[arg(long)]
    pub export_repro: Option<PathBuf>,
    /// Include the actual input files in the repro bundle (synthetic /
    /// shareable data only — never private builds).
    #[arg(long)]
    pub include_inputs: bool,
}

#[derive(clap::Subcommand, Debug)]
pub enum CertifyAction {
    /// Certify that outputs and intermediate files are valid, safe and
    /// byte-identical (signatures, plans, apply, traversal, corruption).
    Integrity {
        /// Old build (file or directory).
        #[arg(long)]
        old: Option<PathBuf>,
        /// New build (same kind as --old).
        #[arg(long)]
        new: Option<PathBuf>,
        /// Verify an existing old signature instead of exporting one.
        #[arg(long)]
        signature_old: Option<PathBuf>,
        /// Verify an existing new signature instead of exporting one.
        #[arg(long)]
        signature_new: Option<PathBuf>,
        /// Verify an existing `.cavsplan` instead of building one.
        #[arg(long)]
        plan: Option<PathBuf>,
        /// Report directory.
        #[arg(long, default_value = "./certification/integrity")]
        out: PathBuf,
        /// Exit 1 instead of 2 when warnings are present.
        #[arg(long)]
        fail_on_warning: bool,
    },
    /// Certify route selection across client states, and measure every
    /// delivery route when tools are available.
    Routes {
        /// Old build (file or directory).
        #[arg(long)]
        old: PathBuf,
        /// New build (same kind as --old).
        #[arg(long)]
        new: PathBuf,
        /// Client states (comma-separated; default: documented matrix).
        #[arg(long)]
        client_states: Option<String>,
        /// Route-selection policy.
        #[arg(long, default_value = "balanced")]
        policy: String,
        /// all (measured matrix) or estimate (planner only).
        #[arg(long, default_value = "all")]
        routes: String,
        /// Path to the butler binary for external comparisons.
        #[arg(long)]
        butler_bin: Option<String>,
        /// Report directory.
        #[arg(long, default_value = "./certification/routes")]
        out: PathBuf,
        /// Exit 1 instead of 2 when warnings are present.
        #[arg(long)]
        fail_on_warning: bool,
    },
    /// Compare current metrics against a baseline and fail on regression.
    Regressions {
        /// Current metrics JSON (a certify routes.json / summary.json).
        #[arg(long)]
        current: PathBuf,
        /// Baseline metrics JSON.
        #[arg(long)]
        baseline: PathBuf,
        /// Max allowed network-bytes regression (e.g. 5%).
        #[arg(long, default_value = "5%")]
        max_network_regression: String,
        /// Max allowed apply-time regression (e.g. 10%).
        #[arg(long, default_value = "10%")]
        max_apply_regression: String,
        /// Max allowed peak-RAM regression (e.g. 20%).
        #[arg(long, default_value = "20%")]
        max_ram_regression: String,
        /// Accept a named regression with an explicit reason: metric=reason
        /// (repeatable).
        #[arg(long)]
        allow_regression: Vec<String>,
        /// Report directory.
        #[arg(long, default_value = "./certification/regressions")]
        out: PathBuf,
        /// Exit 1 instead of 2 when warnings are present.
        #[arg(long)]
        fail_on_warning: bool,
    },
    /// Certify the Godot PCK workflow: byte-identical reconstruction on
    /// every route, PCK analyzer report, optional engine smoke test.
    Godot {
        /// Old .pck file.
        #[arg(long)]
        old_pck: PathBuf,
        /// New .pck file.
        #[arg(long)]
        new_pck: PathBuf,
        /// Path to a Godot binary for the optional smoke test.
        #[arg(long)]
        godot_bin: Option<String>,
        /// Godot test project for the optional smoke test.
        #[arg(long)]
        test_project: Option<PathBuf>,
        /// Godot plugin directory (addons/cavs) for the API surface check.
        #[arg(long)]
        plugin_dir: Option<PathBuf>,
        /// Report directory.
        #[arg(long, default_value = "./certification/godot")]
        out: PathBuf,
        /// Exit 1 instead of 2 when warnings are present.
        #[arg(long)]
        fail_on_warning: bool,
    },
    /// Certify workspace/depot/branch/build workflows and install plans.
    Workspace {
        /// Workspace directory.
        #[arg(long, default_value = "./cavs-workspace")]
        workspace: PathBuf,
        /// App id (default: the workspace default app).
        #[arg(long)]
        app: Option<String>,
        /// Source build id.
        #[arg(long)]
        from: Option<String>,
        /// Target build id (default: the latest build).
        #[arg(long)]
        to: Option<String>,
        /// Report directory.
        #[arg(long, default_value = "./certification/workspace")]
        out: PathBuf,
        /// Exit 1 instead of 2 when warnings are present.
        #[arg(long)]
        fail_on_warning: bool,
    },
    /// Export a reproducibility bundle from a certification directory.
    ExportRepro {
        /// The certification directory to bundle.
        #[arg(long)]
        certification: PathBuf,
        /// Output bundle path (tar.zst).
        #[arg(long)]
        out: PathBuf,
        /// Include actual input files (synthetic / shareable data only).
        #[arg(long)]
        include_inputs: bool,
    },
}

// ---------------------------------------------------------------------------
// Dependency detection
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, serde::Serialize)]
pub struct DependencyStatus {
    pub name: String,
    pub binary: String,
    pub available: bool,
    pub version: Option<String>,
    pub required: bool,
}

pub fn detect_dependencies(
    butler: Option<&str>,
    xdelta3: Option<&str>,
    bsdiff: Option<&str>,
    godot: Option<&str>,
    required_if_configured: bool,
) -> Vec<DependencyStatus> {
    let probe = |name: &str, bin: Option<&str>, default: &str, flag: &str| {
        let binary = bin.unwrap_or(default).to_string();
        let available = crate::tool_metrics::available(&binary);
        DependencyStatus {
            name: name.into(),
            binary: binary.clone(),
            available,
            version: if available {
                crate::tool_metrics::version_line(&binary, flag)
            } else {
                None
            },
            required: required_if_configured && bin.is_some(),
        }
    };
    vec![
        probe("butler", butler, "butler", "--version"),
        probe("xdelta3", xdelta3, "xdelta3", "-V"),
        probe("bsdiff", bsdiff, "bsdiff", ""),
        probe("godot", godot, "godot", "--version"),
    ]
}

// ---------------------------------------------------------------------------
// Sections and the summary report
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, serde::Serialize)]
pub struct Section {
    pub name: String,
    pub result: CheckResult,
    pub rows: Vec<CheckRow>,
    pub report: Option<String>,
}

#[derive(serde::Serialize)]
pub struct Summary {
    pub schema: &'static str,
    pub cavs_version: &'static str,
    pub result: CheckResult,
    pub profile: &'static str,
    pub mode: String,
    pub old: String,
    pub new: String,
    pub recommended_route: String,
    pub reason: Vec<String>,
    pub sections: Vec<Section>,
    pub metrics: BTreeMap<String, f64>,
    pub byte_identical: bool,
    pub exit_code: i32,
    pub note: &'static str,
}

pub const SUMMARY_NOTE: &str = "CAVS certifies updates locally before release. CAVS is not a CDN, \
     marketplace, SaaS, DRM system or storefront; SteamPipe-style figures are \
     estimates from a public model, never Valve's implementation.";

pub fn summary_markdown(s: &Summary) -> String {
    let mut md = String::new();
    md.push_str("# CAVS Certification Report\n\n");
    md.push_str(&format!("Result: **{}**\n\n", s.result.label()));
    md.push_str(&format!("Profile: `{}` · Mode: {}\n\n", s.profile, s.mode));
    md.push_str(&format!(
        "Old build:\n  {}\n\nNew build:\n  {}\n\n",
        s.old, s.new
    ));
    if !s.recommended_route.is_empty() {
        md.push_str(&format!(
            "Recommended route:\n  {}\n\n",
            s.recommended_route
        ));
        if !s.reason.is_empty() {
            md.push_str("Why:\n");
            for r in &s.reason {
                md.push_str(&format!("  - {r}\n"));
            }
            md.push('\n');
        }
    }
    md.push_str("Checks:\n");
    for sec in &s.sections {
        md.push_str(&format!("  {}: {}\n", sec.name, sec.result.label()));
    }
    md.push('\n');
    for sec in &s.sections {
        md.push_str(&format!("## {}\n\n", sec.name));
        md.push_str(&rows_markdown(&sec.rows));
        if let Some(r) = &sec.report {
            md.push_str(&format!("\nDetailed report: `{r}`\n"));
        }
        md.push('\n');
    }
    if !s.metrics.is_empty() {
        md.push_str("## Metrics\n\n| Metric | Value |\n|---|---:|\n");
        for (k, v) in &s.metrics {
            let shown = if k.ends_with("_bytes") {
                human_bytes(*v as u64)
            } else if k.ends_with("_ms") {
                format!("{v:.0} ms")
            } else {
                format!("{v}")
            };
            md.push_str(&format!("| {k} | {shown} |\n"));
        }
        md.push('\n');
    }
    md.push_str(&format!("---\n\n{}\n", s.note));
    md
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Entry point from main: returns the process exit code.
pub fn dispatch(action: Option<CertifyAction>, full: &FullArgs) -> i32 {
    let outcome = match action {
        None => run_full(full),
        Some(CertifyAction::Integrity {
            old,
            new,
            signature_old,
            signature_new,
            plan,
            out,
            fail_on_warning,
        }) => run_integrity_cmd(
            old.as_deref(),
            new.as_deref(),
            signature_old.as_deref(),
            signature_new.as_deref(),
            plan.as_deref(),
            &out,
            fail_on_warning,
        ),
        Some(CertifyAction::Routes {
            old,
            new,
            client_states,
            policy,
            routes,
            butler_bin,
            out,
            fail_on_warning,
        }) => run_routes_cmd(
            &old,
            &new,
            client_states.as_deref(),
            &policy,
            &routes,
            butler_bin.as_deref(),
            &out,
            fail_on_warning,
        ),
        Some(CertifyAction::Regressions {
            current,
            baseline,
            max_network_regression,
            max_apply_regression,
            max_ram_regression,
            allow_regression,
            out,
            fail_on_warning,
        }) => run_regressions_cmd(
            &current,
            &baseline,
            &max_network_regression,
            &max_apply_regression,
            &max_ram_regression,
            &allow_regression,
            &out,
            fail_on_warning,
        ),
        Some(CertifyAction::Godot {
            old_pck,
            new_pck,
            godot_bin,
            test_project,
            plugin_dir,
            out,
            fail_on_warning,
        }) => run_godot_cmd(
            &old_pck,
            &new_pck,
            godot_bin.as_deref(),
            test_project.as_deref(),
            plugin_dir.as_deref(),
            &out,
            fail_on_warning,
        ),
        Some(CertifyAction::Workspace {
            workspace,
            app,
            from,
            to,
            out,
            fail_on_warning,
        }) => run_workspace_cmd(
            &workspace,
            app.as_deref(),
            from.as_deref(),
            to.as_deref(),
            &out,
            fail_on_warning,
        ),
        Some(CertifyAction::ExportRepro {
            certification,
            out,
            include_inputs,
        }) => run_export_repro_cmd(&certification, &out, include_inputs),
    };
    match outcome {
        Ok(code) => code,
        Err(e) => {
            let msg = format!("{e:#}");
            eprintln!("certify: {msg}");
            if msg.contains("CAVS-E-CERTIFY-INPUT") || msg.contains("CAVS-E-CERTIFY-PROFILE") {
                EXIT_INVALID_INPUT
            } else if msg.contains("CAVS-E-CERTIFY-DEP") {
                EXIT_MISSING_DEP
            } else {
                EXIT_INTERNAL
            }
        }
    }
}

/// Environment + dependency snapshot for standalone subcommand runs, so
/// their report directories are as self-describing as a full run's.
fn write_environment(out: &Path) -> Result<()> {
    let env = crate::bench_env::capture(0);
    std::fs::write(
        out.join("environment.json"),
        serde_json::to_vec_pretty(&env)?,
    )?;
    let deps = detect_dependencies(None, None, None, None, false);
    std::fs::write(
        out.join("dependencies.json"),
        serde_json::to_vec_pretty(&deps)?,
    )?;
    Ok(())
}

fn result_to_exit(result: CheckResult, fail_on_warning: bool) -> i32 {
    match result {
        CheckResult::Pass | CheckResult::Skipped => EXIT_PASS,
        CheckResult::Warn => {
            if fail_on_warning {
                EXIT_FAIL
            } else {
                EXIT_WARN
            }
        }
        CheckResult::Fail => EXIT_FAIL,
    }
}

// ---------------------------------------------------------------------------
// Standalone subcommand runners
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn run_integrity_cmd(
    old: Option<&Path>,
    new: Option<&Path>,
    signature_old: Option<&Path>,
    signature_new: Option<&Path>,
    plan: Option<&Path>,
    out: &Path,
    fail_on_warning: bool,
) -> Result<i32> {
    if old.is_none()
        && new.is_none()
        && signature_old.is_none()
        && signature_new.is_none()
        && plan.is_none()
    {
        bail!("CAVS-E-CERTIFY-INPUT: pass --old/--new builds or existing --signature-*/--plan artifacts");
    }
    if let (Some(o), Some(n)) = (old, new) {
        validate_pair(o, n)?;
    }
    std::fs::create_dir_all(out)?;
    write_environment(out)?;
    let mut commands = Vec::new();
    let inputs = integrity::Inputs {
        old,
        new,
        signature_old,
        signature_new,
        plan,
        corruption_checks: true,
        noop_check: true,
    };
    let outcome = integrity::run(&inputs, out, &mut commands)?;
    integrity::write_reports(&outcome, out)?;
    write_commands(out, &commands)?;
    println!("integrity : {}", outcome.result.label());
    println!("reports   : {}", out.display());
    Ok(result_to_exit(outcome.result, fail_on_warning))
}

#[allow(clippy::too_many_arguments)]
fn run_routes_cmd(
    old: &Path,
    new: &Path,
    client_states: Option<&str>,
    policy: &str,
    routes_mode: &str,
    butler_bin: Option<&str>,
    out: &Path,
    fail_on_warning: bool,
) -> Result<i32> {
    validate_pair(old, new)?;
    std::fs::create_dir_all(out)?;
    write_environment(out)?;
    let mut commands = Vec::new();
    let args = routes::Args {
        old,
        new,
        plan: None,
        client_states,
        policy,
        measured: routes_mode == "all",
        butler_bin,
        byte_identical: None,
    };
    let outcome = routes::run(&args, out, &mut commands)?;
    routes::write_reports(&outcome, out)?;
    write_commands(out, &commands)?;
    println!("routes    : {}", outcome.result.label());
    println!("recommended: {} — {}", outcome.recommended, outcome.reason);
    println!("reports   : {}", out.display());
    Ok(result_to_exit(outcome.result, fail_on_warning))
}

#[allow(clippy::too_many_arguments)]
fn run_regressions_cmd(
    current: &Path,
    baseline: &Path,
    max_network: &str,
    max_apply: &str,
    max_ram: &str,
    allow: &[String],
    out: &Path,
    fail_on_warning: bool,
) -> Result<i32> {
    for p in [current, baseline] {
        if !p.exists() {
            bail!("CAVS-E-CERTIFY-INPUT: {} does not exist", p.display());
        }
    }
    std::fs::create_dir_all(out)?;
    write_environment(out)?;
    let thresholds = regressions::Thresholds::parse(max_network, max_apply, max_ram)?;
    let (cur_metrics, cur_bi) = regressions::load_metrics(current)?;
    let (base_metrics, base_bi) = regressions::load_metrics(baseline)?;
    let outcome = regressions::compare(
        &cur_metrics,
        cur_bi,
        &base_metrics,
        base_bi,
        &thresholds,
        allow,
    )?;
    regressions::write_reports(&outcome, out)?;
    println!("regressions: {}", outcome.result.label());
    println!("reports    : {}", out.display());
    Ok(result_to_exit(outcome.result, fail_on_warning))
}

fn run_godot_cmd(
    old_pck: &Path,
    new_pck: &Path,
    godot_bin: Option<&str>,
    test_project: Option<&Path>,
    plugin_dir: Option<&Path>,
    out: &Path,
    fail_on_warning: bool,
) -> Result<i32> {
    validate_pair(old_pck, new_pck)?;
    std::fs::create_dir_all(out)?;
    write_environment(out)?;
    let mut commands = Vec::new();
    let args = godot::Args {
        old_pck,
        new_pck,
        godot_bin,
        test_project,
        plugin_dir,
    };
    let outcome = godot::run(&args, out, &mut commands)?;
    godot::write_reports(&outcome, out)?;
    write_commands(out, &commands)?;
    println!("godot     : {}", outcome.result.label());
    println!("reports   : {}", out.display());
    Ok(result_to_exit(outcome.result, fail_on_warning))
}

fn run_workspace_cmd(
    ws: &Path,
    app: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    out: &Path,
    fail_on_warning: bool,
) -> Result<i32> {
    if !ws.exists() {
        bail!(
            "CAVS-E-CERTIFY-INPUT: workspace {} does not exist",
            ws.display()
        );
    }
    std::fs::create_dir_all(out)?;
    write_environment(out)?;
    let mut commands = Vec::new();
    let outcome = workspace::run(ws, app, from, to, out, &mut commands)?;
    workspace::write_reports(&outcome, out)?;
    write_commands(out, &commands)?;
    println!("workspace : {}", outcome.result.label());
    println!("reports   : {}", out.display());
    Ok(result_to_exit(outcome.result, fail_on_warning))
}

fn run_export_repro_cmd(certification: &Path, out: &Path, include_inputs: bool) -> Result<i32> {
    if !certification.is_dir() {
        bail!(
            "CAVS-E-CERTIFY-INPUT: certification directory {} does not exist",
            certification.display()
        );
    }
    let rows = repro::export(certification, out, include_inputs)?;
    for r in &rows {
        println!("{:<28} {}  {}", r.name, r.result.label(), r.details);
    }
    println!("bundle    : {}", out.display());
    Ok(result_to_exit(worst(&rows), false))
}

// ---------------------------------------------------------------------------
// Full orchestrator
// ---------------------------------------------------------------------------

fn validate_pair(old: &Path, new: &Path) -> Result<()> {
    if !old.exists() {
        bail!(
            "CAVS-E-CERTIFY-INPUT: --old {} does not exist",
            old.display()
        );
    }
    if !new.exists() {
        bail!(
            "CAVS-E-CERTIFY-INPUT: --new {} does not exist",
            new.display()
        );
    }
    if old.is_dir() != new.is_dir() {
        bail!("CAVS-E-CERTIFY-INPUT: --old and --new must both be files or both be directories");
    }
    Ok(())
}

fn is_pck(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("pck"))
        .unwrap_or(false)
}

fn run_full(args: &FullArgs) -> Result<i32> {
    let profile = Profile::parse(&args.profile)?;
    let workspace_mode = args.workspace.is_some();
    if !workspace_mode && (args.old.is_none() || args.new.is_none()) {
        bail!("CAVS-E-CERTIFY-INPUT: pass --old/--new builds, or --workspace with --from/--to");
    }
    if workspace_mode && (args.from.is_none() || args.to.is_none()) {
        bail!("CAVS-E-CERTIFY-INPUT: workspace mode needs --from and --to build ids");
    }
    if let (Some(old), Some(new)) = (args.old.as_deref(), args.new.as_deref()) {
        validate_pair(old, new)?;
    }
    if let Some(ws) = args.workspace.as_deref() {
        if !ws.exists() {
            bail!(
                "CAVS-E-CERTIFY-INPUT: workspace {} does not exist",
                ws.display()
            );
        }
    }
    std::fs::create_dir_all(&args.out)?;

    // -- Dependencies -------------------------------------------------------
    let deps = detect_dependencies(
        args.butler_bin.as_deref(),
        args.xdelta3_bin.as_deref(),
        args.bsdiff_bin.as_deref(),
        args.godot_bin.as_deref(),
        profile.tools_required(),
    );
    std::fs::write(
        args.out.join("dependencies.json"),
        serde_json::to_vec_pretty(&deps)?,
    )?;
    if let Some(missing) = deps.iter().find(|d| d.required && !d.available) {
        bail!(
            "CAVS-E-CERTIFY-DEP-MISSING: required tool '{}' ({}) is not available",
            missing.name,
            missing.binary
        );
    }

    // -- Environment --------------------------------------------------------
    let env = crate::bench_env::capture(0);
    std::fs::write(
        args.out.join("environment.json"),
        serde_json::to_vec_pretty(&env)?,
    )?;

    let mut commands: Vec<String> = Vec::new();
    let mut sections: Vec<Section> = Vec::new();
    let mut metrics: BTreeMap<String, f64> = BTreeMap::new();
    let mut byte_identical = true;
    let mut recommended = String::new();
    let mut reason: Vec<String> = Vec::new();

    let (mode_label, old_label, new_label);

    if workspace_mode {
        // ---- Workspace mode -------------------------------------------------
        let ws = args.workspace.as_deref().unwrap();
        mode_label = "workspace".to_string();
        old_label = args.from.clone().unwrap_or_default();
        new_label = args.to.clone().unwrap_or_default();

        let ws_outcome = workspace::run(
            ws,
            args.app.as_deref(),
            args.from.as_deref(),
            args.to.as_deref(),
            &args.out,
            &mut commands,
        )?;
        workspace::write_reports(&ws_outcome, &args.out)?;
        sections.push(Section {
            name: "Workspace/install-plan".into(),
            result: ws_outcome.result,
            rows: ws_outcome.rows.clone(),
            report: Some("workspace.md".into()),
        });

        // Integrity per depot present in both builds.
        let mut int_rows: Vec<CheckRow> = Vec::new();
        for (depot, old_src, new_src) in &ws_outcome.depot_pairs {
            let sub = args.out.join("depots").join(depot);
            std::fs::create_dir_all(&sub)?;
            let inputs = integrity::Inputs {
                old: Some(old_src),
                new: Some(new_src),
                signature_old: None,
                signature_new: None,
                plan: None,
                corruption_checks: profile.corruption(),
                noop_check: profile != Profile::Quick,
            };
            match integrity::run(&inputs, &sub, &mut commands) {
                Ok(o) => {
                    integrity::write_reports(&o, &sub)?;
                    byte_identical &= o.byte_identical;
                    int_rows.push(CheckRow::new(
                        &format!("depot {depot}"),
                        o.result,
                        format!("byte-identical: {}", o.byte_identical),
                    ));
                }
                Err(e) => int_rows.push(CheckRow::new(
                    &format!("depot {depot}"),
                    CheckResult::Fail,
                    format!("{e:#}"),
                )),
            }
        }
        if int_rows.is_empty() {
            int_rows.push(CheckRow::new(
                "depot integrity",
                CheckResult::Skipped,
                "no depot present in both builds",
            ));
        }
        sections.push(Section {
            name: "Integrity".into(),
            result: worst(&int_rows),
            rows: int_rows,
            report: None,
        });
    } else {
        // ---- Path mode ------------------------------------------------------
        let old = args.old.as_deref().unwrap();
        let new = args.new.as_deref().unwrap();
        validate_pair(old, new)?;
        let godot_pair = args.engine == "godot" || (is_pck(old) && is_pck(new));
        mode_label = if godot_pair {
            "godot-pck".into()
        } else if old.is_dir() {
            "directory".into()
        } else {
            "artifact".into()
        };
        old_label = old.display().to_string();
        new_label = new.display().to_string();

        // Integrity ----------------------------------------------------------
        let inputs = integrity::Inputs {
            old: Some(old),
            new: Some(new),
            signature_old: None,
            signature_new: None,
            plan: None,
            corruption_checks: profile.corruption(),
            noop_check: profile != Profile::Quick,
        };
        let int = integrity::run(&inputs, &args.out, &mut commands)?;
        integrity::write_reports(&int, &args.out)?;
        byte_identical = int.byte_identical;
        metrics.extend(int.metrics.clone());
        sections.push(Section {
            name: "Integrity".into(),
            result: int.result,
            rows: int.rows.clone(),
            report: Some("integrity.md".into()),
        });

        // Routes ---------------------------------------------------------------
        let route_args = routes::Args {
            old,
            new,
            plan: int.plan_path.as_deref(),
            client_states: args.client_states.as_deref(),
            policy: &args.policy,
            measured: profile.measured_routes() && args.routes == "all",
            butler_bin: args.butler_bin.as_deref(),
            byte_identical: Some(byte_identical),
        };
        let rt = routes::run(&route_args, &args.out, &mut commands)?;
        routes::write_reports(&rt, &args.out)?;
        metrics.extend(rt.metrics.clone());
        recommended = rt.recommended.clone();
        reason = rt.reasons.clone();
        sections.push(Section {
            name: "Routes".into(),
            result: rt.result,
            rows: rt.rows.clone(),
            report: Some("routes.md".into()),
        });

        // Analysis (SteamPipe-style, packs, disk I/O) ---------------------------
        if profile.analysis() {
            let rows = run_analysis(old, new, &args.out, &mut commands)?;
            sections.push(Section {
                name: "SteamPipe-style analysis".into(),
                result: worst(&rows),
                rows,
                report: Some("steampipe-style.md".into()),
            });
        }

        // Godot -----------------------------------------------------------------
        if godot_pair {
            let g_args = godot::Args {
                old_pck: old,
                new_pck: new,
                godot_bin: args.godot_bin.as_deref(),
                test_project: args.test_project.as_deref(),
                plugin_dir: None,
            };
            let g = godot::run_with(
                &g_args,
                Some(godot::Precomputed {
                    byte_identical,
                    measured: rt.measured.as_ref(),
                }),
                &args.out,
                &mut commands,
            )?;
            godot::write_reports(&g, &args.out)?;
            byte_identical &= g.byte_identical;
            sections.push(Section {
                name: "Godot compatibility".into(),
                result: g.result,
                rows: g.rows.clone(),
                report: Some("godot.md".into()),
            });
        }
    }

    // Regressions (any mode) ---------------------------------------------------
    if profile.regressions() {
        if let Some(baseline) = &args.baseline {
            let thresholds = regressions::Thresholds::parse(
                &args.max_network_regression,
                &args.max_apply_regression,
                &args.max_ram_regression,
            )?;
            let (base_metrics, base_bi) = regressions::load_metrics(baseline)?;
            let reg = regressions::compare(
                &metrics,
                byte_identical,
                &base_metrics,
                base_bi,
                &thresholds,
                &args.allow_regression,
            )?;
            regressions::write_reports(&reg, &args.out)?;
            commands.push(format!(
                "cavs certify regressions --current {}/routes.json --baseline {} \
                 --max-network-regression {} --max-apply-regression {} --max-ram-regression {}",
                args.out.display(),
                baseline.display(),
                args.max_network_regression,
                args.max_apply_regression,
                args.max_ram_regression
            ));
            sections.push(Section {
                name: "Regression".into(),
                result: reg.result,
                rows: reg.rows.clone(),
                report: Some("regressions.md".into()),
            });
        } else {
            sections.push(Section {
                name: "Regression".into(),
                result: CheckResult::Skipped,
                rows: vec![CheckRow::new(
                    "baseline",
                    CheckResult::Skipped,
                    "no --baseline provided",
                )],
                report: None,
            });
        }
    }

    if let Some(path) = &args.save_baseline {
        regressions::write_baseline(&metrics, byte_identical, path)?;
        println!("baseline  : {}", path.display());
    }

    // Summary --------------------------------------------------------------------
    let overall = sections
        .iter()
        .map(|s| s.result)
        .filter(|r| *r != CheckResult::Skipped)
        .max()
        .unwrap_or(CheckResult::Pass);
    let exit_code = result_to_exit(overall, args.fail_on_warning);
    let summary = Summary {
        schema: SUMMARY_SCHEMA,
        cavs_version: env!("CARGO_PKG_VERSION"),
        result: overall,
        profile: profile.name(),
        mode: mode_label,
        old: old_label,
        new: new_label,
        recommended_route: recommended,
        reason,
        sections,
        metrics,
        byte_identical,
        exit_code,
        note: SUMMARY_NOTE,
    };
    std::fs::write(
        args.out.join("summary.json"),
        serde_json::to_vec_pretty(&summary)?,
    )?;
    std::fs::write(args.out.join("summary.md"), summary_markdown(&summary))?;
    if let Some(json_out) = &args.json_out {
        std::fs::write(json_out, serde_json::to_vec_pretty(&summary)?)?;
    }
    write_commands(&args.out, &commands)?;

    // Repro bundle ----------------------------------------------------------------
    if profile.repro() || args.export_repro.is_some() {
        let default_bundle = args.out.join("repro.tar.zst");
        let bundle = args.export_repro.as_deref().unwrap_or(&default_bundle);
        let rows = repro::export(&args.out, bundle, args.include_inputs)?;
        println!(
            "repro     : {} ({})",
            bundle.display(),
            worst(&rows).label()
        );
    }

    println!();
    println!("Result: {}", summary.result.label());
    for sec in &summary.sections {
        println!("  {:<28} {}", sec.name, sec.result.label());
    }
    if !summary.recommended_route.is_empty() {
        println!("Recommended route: {}", summary.recommended_route);
    }
    println!("Reports: {}", args.out.display());
    Ok(exit_code)
}

/// SteamPipe-style analysis + pack analysis + disk I/O estimate sections.
fn run_analysis(
    old: &Path,
    new: &Path,
    out: &Path,
    commands: &mut Vec<String>,
) -> Result<Vec<CheckRow>> {
    use cavs_analyzer::detect::Thresholds;
    use cavs_analyzer::Engine;
    let mut rows = Vec::new();

    let analysis =
        cavs_analyzer::compare::analyze(old, new, Engine::Auto, &Thresholds::default(), &|_| true)
            .context("steampipe-style analysis failed")?;
    std::fs::write(
        out.join("steampipe-style.md"),
        crate::steampipe_cmd::analysis_markdown(&analysis),
    )?;
    commands.push(format!(
        "cavs analyze steampipe {} {} --out steampipe-style.md",
        old.display(),
        new.display()
    ));
    let sev_warn = analysis
        .findings
        .iter()
        .filter(|f| f.severity != cavs_analyzer::detect::Severity::Info)
        .count();
    rows.push(CheckRow::new(
        "steampipe-style analysis",
        if sev_warn > 0 {
            CheckResult::Warn
        } else {
            CheckResult::Pass
        },
        format!(
            "{} findings ({} non-info); est. download {}",
            analysis.findings.len(),
            sev_warn,
            human_bytes(analysis.estimated_steampipe_download)
        ),
    ));

    let packs = crate::analyze_packs::analyze_packs(&crate::analyze_packs::PacksArgs {
        old,
        new,
        engine: "auto",
        out: Some(&out.join("pack-analysis.md")),
        json: false,
    });
    commands.push(format!(
        "cavs analyze-packs {} {} --out pack-analysis.md",
        old.display(),
        new.display()
    ));
    rows.push(match packs {
        Ok(()) => CheckRow::new("pack analysis", CheckResult::Pass, "pack-analysis.md"),
        Err(e) => CheckRow::new("pack analysis", CheckResult::Warn, format!("{e:#}")),
    });

    let profiles = crate::io_estimate::default_profiles();
    let io_routes = crate::io_estimate::routes_from_analysis(&analysis, &profiles);
    let io_report = crate::io_estimate::IoReport {
        old: old.display().to_string(),
        new: new.display().to_string(),
        routes: io_routes,
        note: "estimates from the SteamPipe-style analysis model".into(),
    };
    std::fs::write(
        out.join("io-estimate.md"),
        crate::io_estimate::markdown(&io_report),
    )?;
    commands.push(format!(
        "cavs io-estimate {} {} --out io-estimate.md",
        old.display(),
        new.display()
    ));
    rows.push(CheckRow::new(
        "disk I/O estimate",
        CheckResult::Pass,
        format!("{} routes estimated", io_report.routes.len()),
    ));

    Ok(rows)
}

pub fn write_commands(out: &Path, commands: &[String]) -> Result<()> {
    let mut sh = String::from(
        "#!/bin/sh\n# Commands equivalent to this certification run.\n# Generated by `cavs certify` — reproduce by running them in order.\nset -e\n\n",
    );
    for c in commands {
        sh.push_str(c);
        sh.push('\n');
    }
    std::fs::write(out.join("commands.sh"), sh)?;
    Ok(())
}
