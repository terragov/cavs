//! Traffic models: how users actually move between versions (v1.1.0).
//!
//! A policy comparison is meaningless without an update-behavior
//! assumption — adjacent diffs look perfect if everyone updates every
//! release and terrible if half the clients return after five patches.
//! Built-in models cover the common shapes; custom TOML files describe
//! measured telemetry.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrafficModel {
    pub name: String,
    pub users: u64,
    pub rules: Vec<TrafficRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrafficRule {
    pub kind: String,
    #[serde(default)]
    pub min_skip: usize,
    #[serde(default)]
    pub max_skip: usize,
    #[serde(default)]
    pub min_age: usize,
    pub probability: f64,
}

/// TOML wrapper matching the documented format:
/// `[traffic]` header + `[[traffic.rule]]` entries.
#[derive(Deserialize)]
struct TrafficFile {
    traffic: TrafficToml,
}
#[derive(Deserialize)]
struct TrafficToml {
    name: String,
    #[serde(default = "default_users")]
    users: u64,
    #[serde(default, rename = "rule")]
    rules: Vec<TrafficRule>,
}
fn default_users() -> u64 {
    100_000
}

fn rule(kind: &str, probability: f64) -> TrafficRule {
    TrafficRule {
        kind: kind.into(),
        min_skip: 0,
        max_skip: 0,
        min_age: 0,
        probability,
    }
}

fn skip(min: usize, max: usize, probability: f64) -> TrafficRule {
    TrafficRule {
        kind: "skip_range".into(),
        min_skip: min,
        max_skip: max,
        min_age: 0,
        probability,
    }
}

fn old_to_latest(min_age: usize, probability: f64) -> TrafficRule {
    TrafficRule {
        kind: "old_to_latest".into(),
        min_skip: 0,
        max_skip: 0,
        min_age,
        probability,
    }
}

pub fn builtin(name: &str) -> Option<TrafficModel> {
    let rules = match name {
        "adjacent-heavy" => vec![
            rule("adjacent", 0.80),
            skip(2, 4, 0.15),
            old_to_latest(6, 0.04),
            rule("reinstall_latest", 0.01),
        ],
        "skip-heavy" => vec![
            rule("adjacent", 0.40),
            skip(2, 8, 0.40),
            old_to_latest(6, 0.15),
            rule("reinstall_latest", 0.05),
        ],
        "live-service-weekly" => vec![
            rule("adjacent", 0.65),
            skip(2, 3, 0.25),
            old_to_latest(6, 0.08),
            rule("reinstall_latest", 0.02),
        ],
        "major-release" => vec![
            rule("adjacent", 0.30),
            skip(2, 5, 0.20),
            old_to_latest(4, 0.45),
            rule("reinstall_latest", 0.05),
        ],
        "random" => vec![skip(1, usize::MAX, 1.0)],
        _ => return None,
    };
    Some(TrafficModel {
        name: name.into(),
        users: default_users(),
        rules,
    })
}

pub const BUILTIN_NAMES: &[&str] = &[
    "adjacent-heavy",
    "skip-heavy",
    "live-service-weekly",
    "major-release",
    "random",
];

/// `adjacent-heavy` | `custom:model.toml` | a path ending in .toml.
pub fn load(spec: &str) -> Result<TrafficModel> {
    if let Some(m) = builtin(spec) {
        return Ok(m);
    }
    let path = spec.strip_prefix("custom:").unwrap_or(spec);
    if path.ends_with(".toml") {
        return load_toml(Path::new(path));
    }
    bail!(
        "unknown traffic model {spec:?} (built-ins: {}; or custom:file.toml)",
        BUILTIN_NAMES.join(", ")
    )
}

pub fn load_toml(path: &Path) -> Result<TrafficModel> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read traffic model {}", path.display()))?;
    let file: TrafficFile =
        toml::from_str(&text).with_context(|| format!("bad traffic TOML {}", path.display()))?;
    let model = TrafficModel {
        name: file.traffic.name,
        users: file.traffic.users,
        rules: file.traffic.rules,
    };
    if model.rules.is_empty() {
        bail!(
            "traffic model {} has no [[traffic.rule]] entries",
            model.name
        );
    }
    Ok(model)
}

/// One user movement the simulator prices under every policy.
/// `from == to` encodes a reinstall of the latest version.
#[derive(Debug, Clone, Serialize)]
pub struct WeightedQuery {
    pub from: usize,
    pub to: usize,
    pub probability: f64,
    pub rule: String,
}

/// Expand a model into deterministic weighted queries over N versions.
/// Each rule's probability is spread uniformly across the (from,to)
/// pairs it matches; rules that match nothing are dropped and the rest
/// renormalized, so the result always sums to ~1.
pub fn expand(model: &TrafficModel, n: usize) -> Result<Vec<WeightedQuery>> {
    if n < 2 {
        bail!("traffic simulation needs at least two versions");
    }
    let latest = n - 1;
    let mut queries: Vec<WeightedQuery> = Vec::new();
    for r in &model.rules {
        let pairs: Vec<(usize, usize)> = match r.kind.as_str() {
            "adjacent" => (0..latest).map(|i| (i, i + 1)).collect(),
            "skip_range" => {
                let lo = r.min_skip.max(1);
                let mut v = Vec::new();
                for from in 0..latest {
                    for to in from + 1..n {
                        let dist = to - from;
                        if dist >= lo && dist <= r.max_skip.max(lo) {
                            v.push((from, to));
                        }
                    }
                }
                v
            }
            "old_to_latest" => (0..latest)
                .filter(|&i| latest - i >= r.min_age.max(1))
                .map(|i| (i, latest))
                .collect(),
            "reinstall_latest" => vec![(latest, latest)],
            other => bail!("unknown traffic rule kind {other:?}"),
        };
        if pairs.is_empty() {
            continue;
        }
        let each = r.probability / pairs.len() as f64;
        for (from, to) in pairs {
            queries.push(WeightedQuery {
                from,
                to,
                probability: each,
                rule: r.kind.clone(),
            });
        }
    }
    if queries.is_empty() {
        bail!("traffic model {} matches no version pairs", model.name);
    }
    let total: f64 = queries.iter().map(|q| q.probability).sum();
    for q in &mut queries {
        q.probability /= total;
    }
    // Merge duplicates (two rules can hit the same pair).
    queries.sort_by_key(|q| (q.from, q.to));
    let mut merged: Vec<WeightedQuery> = Vec::new();
    for q in queries {
        match merged.last_mut() {
            Some(last) if last.from == q.from && last.to == q.to => {
                last.probability += q.probability;
                if !last.rule.contains(&q.rule) {
                    last.rule = format!("{}+{}", last.rule, q.rule);
                }
            }
            _ => merged.push(q),
        }
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_expand_and_normalize() {
        for name in BUILTIN_NAMES {
            let model = builtin(name).unwrap();
            let queries = expand(&model, 10).unwrap();
            let total: f64 = queries.iter().map(|q| q.probability).sum();
            assert!((total - 1.0).abs() < 1e-9, "{name} sums to {total}");
            assert!(queries.iter().all(|q| q.from <= q.to));
        }
    }

    #[test]
    fn adjacent_heavy_weights_adjacent_pairs_most() {
        let model = builtin("adjacent-heavy").unwrap();
        let queries = expand(&model, 10).unwrap();
        let adjacent: f64 = queries
            .iter()
            .filter(|q| q.to - q.from == 1)
            .map(|q| q.probability)
            .sum();
        assert!(adjacent > 0.75, "adjacent share {adjacent}");
    }

    #[test]
    fn custom_toml_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("traffic.toml");
        std::fs::write(
            &path,
            r#"
[traffic]
name = "adjacent-heavy-live-service"
users = 100000

[[traffic.rule]]
kind = "adjacent"
probability = 0.70

[[traffic.rule]]
kind = "skip_range"
min_skip = 2
max_skip = 5
probability = 0.20

[[traffic.rule]]
kind = "old_to_latest"
min_age = 6
probability = 0.08

[[traffic.rule]]
kind = "reinstall_latest"
probability = 0.02
"#,
        )
        .unwrap();
        let model = load(&format!("custom:{}", path.display())).unwrap();
        assert_eq!(model.users, 100_000);
        assert_eq!(model.rules.len(), 4);
        let queries = expand(&model, 12).unwrap();
        assert!(queries.iter().any(|q| q.from == q.to)); // reinstall present
    }

    #[test]
    fn unknown_model_is_a_clear_error() {
        assert!(load("definitely-not-a-model").is_err());
    }

    #[test]
    fn tiny_streams_drop_unmatchable_rules() {
        // 3 versions: old_to_latest(min_age 6) matches nothing but the
        // model still normalizes.
        let model = builtin("adjacent-heavy").unwrap();
        let queries = expand(&model, 3).unwrap();
        let total: f64 = queries.iter().map(|q| q.probability).sum();
        assert!((total - 1.0).abs() < 1e-9);
    }
}
