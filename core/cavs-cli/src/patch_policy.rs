//! `cavs patch-policy` — decide which old→new pairs deserve an optimized
//! `.cavspatch` sidecar (v0.8.0).
//!
//! Pairwise patches explode combinatorially: N versions have N·(N−1)/2
//! pairs (10 versions → 45, 100 → 4,950). CAVS never generates all of
//! them — the content-addressed store already serves *any* jump. Sidecars
//! are an optimization for **hot pairs** only, chosen by policy:
//!
//! ```toml
//! [optimized_patches]
//! enabled = true
//! max_pairs_per_release = 3
//! max_total_patch_storage_ratio = 0.25
//! pairs = ["previous", "latest-stable", "top-installed"]
//! algorithm = "auto"
//! compression = "auto"
//! expire_after_days = 90
//! ```
//!
//! `previous` pins vN−1→vN; `latest-stable` pins the newest non-prerelease
//! before the target; `top-installed` uses the installed-version
//! distribution (a JSON share map) to cover where clients actually are.
//! Explicit pins (`"v3->v10"`) are honored first.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct PolicyFile {
    pub optimized_patches: Policy,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
pub struct Policy {
    pub enabled: bool,
    pub max_pairs_per_release: usize,
    /// Sidecar storage budget as a fraction of one full release.
    pub max_total_patch_storage_ratio: f64,
    pub pairs: Vec<String>,
    pub algorithm: String,
    pub compression: String,
    pub expire_after_days: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            enabled: true,
            max_pairs_per_release: 3,
            max_total_patch_storage_ratio: 0.25,
            pairs: vec![
                "previous".into(),
                "latest-stable".into(),
                "top-installed".into(),
            ],
            algorithm: "auto".into(),
            compression: "auto".into(),
            expire_after_days: 90,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PlannedPair {
    pub old: String,
    pub new: String,
    pub rule: String,
    pub reason: String,
}

#[derive(Debug, serde::Serialize)]
pub struct PolicyReport {
    pub target: String,
    pub versions: Vec<String>,
    pub all_pairs_possible: u64,
    pub planned: Vec<PlannedPair>,
    pub skipped_rules: Vec<String>,
    pub algorithm: String,
    pub compression: String,
    pub expire_after_days: u32,
}

pub fn load_policy(path: Option<&Path>) -> Result<Policy> {
    match path {
        None => Ok(Policy::default()),
        Some(p) => {
            let text = std::fs::read_to_string(p)
                .with_context(|| format!("cannot read {}", p.display()))?;
            let file: PolicyFile = toml::from_str(&text)
                .with_context(|| format!("bad policy TOML {}", p.display()))?;
            Ok(file.optimized_patches)
        }
    }
}

/// Decide the hot pairs for updating to `versions.last()`.
/// `distribution` maps version → installed share (0..1).
pub fn plan_pairs(
    versions: &[String],
    distribution: Option<&HashMap<String, f64>>,
    policy: &Policy,
) -> Result<PolicyReport> {
    if versions.len() < 2 {
        bail!("need at least two versions (old ones and the release target)");
    }
    let latest = versions.last().unwrap().clone();
    let older = &versions[..versions.len() - 1];
    let n = versions.len() as u64;
    let mut report = PolicyReport {
        target: latest.clone(),
        versions: versions.to_vec(),
        all_pairs_possible: n * (n - 1) / 2,
        planned: Vec::new(),
        skipped_rules: Vec::new(),
        algorithm: policy.algorithm.clone(),
        compression: policy.compression.clone(),
        expire_after_days: policy.expire_after_days,
    };
    if !policy.enabled {
        report.skipped_rules.push("disabled by policy".into());
        return Ok(report);
    }

    let push = |report: &mut PolicyReport, old: &str, rule: &str, reason: String| {
        if report.planned.len() >= policy.max_pairs_per_release {
            return false;
        }
        if old == latest || report.planned.iter().any(|p| p.old == old) {
            return true; // duplicate/self: skip but keep filling
        }
        report.planned.push(PlannedPair {
            old: old.to_string(),
            new: latest.clone(),
            rule: rule.to_string(),
            reason,
        });
        true
    };

    for rule in &policy.pairs {
        match rule.as_str() {
            "previous" => {
                if let Some(prev) = older.last() {
                    push(
                        &mut report,
                        prev,
                        "previous",
                        "the adjacent update most clients take first".into(),
                    );
                }
            }
            "latest-stable" => match older.iter().rev().find(|v| is_stable(v)) {
                Some(stable) => {
                    push(
                        &mut report,
                        stable,
                        "latest-stable",
                        "newest non-prerelease before the target".into(),
                    );
                }
                None => report
                    .skipped_rules
                    .push("latest-stable: no stable version before the target".into()),
            },
            "top-installed" => match distribution {
                Some(dist) => {
                    let mut shares: Vec<(&String, f64)> = older
                        .iter()
                        .map(|v| (v, dist.get(v).copied().unwrap_or(0.0)))
                        .collect();
                    shares.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(b.0)));
                    for (v, share) in shares {
                        if share <= 0.0 {
                            break;
                        }
                        if !push(
                            &mut report,
                            v,
                            "top-installed",
                            format!("{:.0}% of installs", share * 100.0),
                        ) {
                            break;
                        }
                    }
                }
                None => report
                    .skipped_rules
                    .push("top-installed: no --distribution given".into()),
            },
            pinned if pinned.contains("->") => {
                let (old, new) = pinned.split_once("->").unwrap();
                let (old, new) = (old.trim(), new.trim());
                if new == latest && versions.iter().any(|v| v == old) {
                    push(&mut report, old, "pinned", "manually pinned pair".into());
                } else {
                    report
                        .skipped_rules
                        .push(format!("pinned {pinned}: not a known old→target pair"));
                }
            }
            other => report.skipped_rules.push(format!("unknown rule {other}")),
        }
    }
    Ok(report)
}

fn is_stable(version: &str) -> bool {
    !version.contains("beta")
        && !version.contains("alpha")
        && !version.contains("rc")
        && !version.contains('-')
}

pub fn run(
    versions: &str,
    distribution: Option<&Path>,
    config: Option<&Path>,
    json: bool,
) -> Result<()> {
    let versions: Vec<String> = versions
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let dist: Option<HashMap<String, f64>> = match distribution {
        Some(p) => Some(serde_json::from_slice(&std::fs::read(p)?)?),
        None => None,
    };
    let policy = load_policy(config)?;
    let report = plan_pairs(&versions, dist.as_ref(), &policy)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    println!(
        "patch-policy: target {} · {} versions · {} possible pairs → {} sidecars",
        report.target,
        report.versions.len(),
        report.all_pairs_possible,
        report.planned.len()
    );
    for p in &report.planned {
        println!("  {} → {}  [{}] {}", p.old, p.new, p.rule, p.reason);
    }
    for s in &report.skipped_rules {
        println!("  skipped: {s}");
    }
    println!(
        "\ngenerate with: cavs optimize-patch --old <old build> --new <new build> \
         --algo {} --compression {} -o patches/<old>_to_<new>.cavspatch",
        report.algorithm, report.compression
    );
    println!(
        "storage budget: ≤{:.0}% of one release; expire after {} days; \
         every other jump is served by the content-addressed store",
        load_policy(config)?.max_total_patch_storage_ratio * 100.0,
        report.expire_after_days
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versions(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn default_policy_picks_hot_pairs_not_all_pairs() {
        let v = versions(&["v1", "v2", "v3", "v4-beta", "v5"]);
        let mut dist = HashMap::new();
        dist.insert("v1".to_string(), 0.12);
        dist.insert("v2".to_string(), 0.18);
        dist.insert("v3".to_string(), 0.55);
        dist.insert("v4-beta".to_string(), 0.05);
        let report = plan_pairs(&v, Some(&dist), &Policy::default()).unwrap();
        assert_eq!(report.all_pairs_possible, 10);
        assert!(report.planned.len() <= 3);
        // previous = v4-beta, latest-stable = v3, top-installed = v3 (dup) → v2
        let olds: Vec<&str> = report.planned.iter().map(|p| p.old.as_str()).collect();
        assert_eq!(olds, vec!["v4-beta", "v3", "v2"]);
        assert!(report.planned.iter().all(|p| p.new == "v5"));
    }

    #[test]
    fn pinned_pairs_and_caps() {
        let v = versions(&["v1", "v2", "v3"]);
        let policy = Policy {
            max_pairs_per_release: 1,
            pairs: vec!["v1->v3".into(), "previous".into()],
            ..Policy::default()
        };
        let report = plan_pairs(&v, None, &policy).unwrap();
        assert_eq!(report.planned.len(), 1);
        assert_eq!(report.planned[0].old, "v1");
        assert_eq!(report.planned[0].rule, "pinned");
    }

    #[test]
    fn disabled_policy_plans_nothing() {
        let v = versions(&["v1", "v2"]);
        let policy = Policy {
            enabled: false,
            ..Policy::default()
        };
        let report = plan_pairs(&v, None, &policy).unwrap();
        assert!(report.planned.is_empty());
    }
}
