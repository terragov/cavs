//! Workspace commands (v0.9.0): `cavs workspace init`, `cavs depot
//! add/analyze-sharing`, `cavs branch add/promote/rollback/
//! promote-preview`, `cavs build create` and `cavs install-plan`.
//!
//! Everything is local metadata — SteamPipe-like apps/depots/branches/
//! builds without accounts, uploads or a remote service.

use crate::report::human_bytes;
use anyhow::{bail, Context, Result};
use cavs_workspace::{sharing, Branch, BranchVisibility, Depot, DepotIndex, Platform, Workspace};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn open(workspace: &Path) -> Result<Workspace> {
    Workspace::open(workspace)
}

pub fn init(workspace: &Path, app: &str) -> Result<()> {
    Workspace::init(workspace, app)?;
    println!("workspace: {} (app '{app}')", workspace.display());
    println!("next     : cavs depot add <id> · cavs branch add <id> · cavs build create");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn depot_add(
    workspace: &Path,
    app: Option<&str>,
    id: &str,
    name: Option<&str>,
    platform: Option<&str>,
    language: Option<&str>,
    optional: bool,
) -> Result<()> {
    let ws = open(workspace)?;
    let app_id = ws.app_id(app)?;
    let platform = match platform {
        Some(p) => Some(
            Platform::parse(p)
                .ok_or_else(|| anyhow::anyhow!("unknown platform '{p}' (windows|linux|macos)"))?,
        ),
        None => None,
    };
    ws.add_depot(
        &app_id,
        Depot {
            id: id.into(),
            name: name.unwrap_or(id).into(),
            platform,
            language: language.map(String::from),
            optional,
        },
    )?;
    println!("depot    : '{id}' added to app '{app_id}'");
    Ok(())
}

pub fn branch_add(
    workspace: &Path,
    app: Option<&str>,
    id: &str,
    name: Option<&str>,
    private: bool,
) -> Result<()> {
    let ws = open(workspace)?;
    let app_id = ws.app_id(app)?;
    ws.add_branch(
        &app_id,
        Branch {
            id: id.into(),
            name: name.unwrap_or(id).into(),
            visibility: if private {
                BranchVisibility::Private
            } else {
                BranchVisibility::Public
            },
            current_build: None,
            history: Vec::new(),
        },
    )?;
    println!("branch   : '{id}' added to app '{app_id}'");
    Ok(())
}

/// `--depot base=./Build/Base` pairs.
pub fn parse_depot_specs(specs: &[String]) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    for spec in specs {
        let (id, path) = spec
            .split_once('=')
            .with_context(|| format!("--depot expects id=path, got '{spec}'"))?;
        out.push((id.trim().to_string(), PathBuf::from(path.trim())));
    }
    Ok(out)
}

pub fn build_create(
    workspace: &Path,
    app: Option<&str>,
    branch: Option<&str>,
    label: Option<&str>,
    depot_specs: &[String],
) -> Result<()> {
    let ws = open(workspace)?;
    let app_id = ws.app_id(app)?;
    let depots = parse_depot_specs(depot_specs)?;
    let build = ws.create_build(&app_id, label, branch, &depots, now())?;
    println!(
        "build    : {} created ({} depots{})",
        build.id,
        build.depots.len(),
        label.map(|l| format!(", label '{l}'")).unwrap_or_default()
    );
    for d in &build.depots {
        println!(
            "  {:<16} {:>10}  {} files  ({})",
            d.depot_id,
            human_bytes(d.total_bytes),
            d.files,
            d.source_path
        );
    }
    if let Some(b) = branch {
        println!("branch   : '{b}' now points at {}", build.id);
    }
    Ok(())
}

pub fn branch_promote(
    workspace: &Path,
    app: Option<&str>,
    branch: &str,
    build: &str,
) -> Result<()> {
    let ws = open(workspace)?;
    let app_id = ws.app_id(app)?;
    ws.promote(&app_id, branch, build)?;
    println!("promote  : branch '{branch}' → {build}");
    Ok(())
}

pub fn branch_rollback(workspace: &Path, app: Option<&str>, branch: &str, to: &str) -> Result<()> {
    let ws = open(workspace)?;
    let app_id = ws.app_id(app)?;
    ws.rollback(&app_id, branch, to)?;
    println!("rollback : branch '{branch}' → {to}");
    Ok(())
}

// ---------------------------------------------------------------------------
// promote-preview
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct DepotDelta {
    depot: String,
    status: String, // updated | new | removed | unchanged
    old_bytes: u64,
    new_bytes: u64,
    estimated_update_bytes: u64,
}

pub fn branch_promote_preview(
    workspace: &Path,
    app: Option<&str>,
    branch: &str,
    build: &str,
    json: bool,
) -> Result<()> {
    let ws = open(workspace)?;
    let app_id = ws.app_id(app)?;
    let app_meta = ws.load_app(&app_id)?;
    let candidate = ws.build(&app_id, build)?;
    let current_id = app_meta.branch(branch)?.current_build.clone();
    let Some(current_id) = current_id else {
        println!(
            "promote-preview: branch '{branch}' serves nothing yet — promotion is a \
             fresh install of {} for every client",
            build
        );
        return Ok(());
    };
    let current = ws.build(&app_id, &current_id)?;

    let mut rows: Vec<DepotDelta> = Vec::new();
    for db in &candidate.depots {
        let new_idx = ws.depot_index(&app_id, &candidate.id, &db.depot_id)?;
        match current.depots.iter().find(|d| d.depot_id == db.depot_id) {
            Some(_) => {
                let old_idx = ws.depot_index(&app_id, &current.id, &db.depot_id)?;
                let update = update_bytes(&old_idx, &new_idx);
                rows.push(DepotDelta {
                    depot: db.depot_id.clone(),
                    status: if update == 0 { "unchanged" } else { "updated" }.into(),
                    old_bytes: old_idx.total_bytes,
                    new_bytes: new_idx.total_bytes,
                    estimated_update_bytes: update,
                });
            }
            None => rows.push(DepotDelta {
                depot: db.depot_id.clone(),
                status: "new".into(),
                old_bytes: 0,
                new_bytes: new_idx.total_bytes,
                estimated_update_bytes: new_idx.total_bytes,
            }),
        }
    }
    for db in &current.depots {
        if !candidate.depots.iter().any(|d| d.depot_id == db.depot_id) {
            rows.push(DepotDelta {
                depot: db.depot_id.clone(),
                status: "removed".into(),
                old_bytes: db.total_bytes,
                new_bytes: 0,
                estimated_update_bytes: 0,
            });
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    println!(
        "promote-preview: branch '{branch}' {} → {}",
        current.id, candidate.id
    );
    let mut total = 0u64;
    for r in &rows {
        total += r.estimated_update_bytes;
        println!(
            "  {:<16} {:<10} {:>10} → {:>10}   update ~{}",
            r.depot,
            r.status,
            human_bytes(r.old_bytes),
            human_bytes(r.new_bytes),
            human_bytes(r.estimated_update_bytes)
        );
    }
    println!(
        "total    : ~{} estimated update per client",
        human_bytes(total)
    );
    println!("note     : estimates from content indices (raw new chunk bytes)");
    Ok(())
}

/// Raw bytes of chunks in `new` that `old` does not hold.
fn update_bytes(old: &DepotIndex, new: &DepotIndex) -> u64 {
    let mut old_chunks: HashMap<&str, ()> = HashMap::new();
    for chunks in old.files.values() {
        for (hash, _) in chunks {
            old_chunks.insert(hash.as_str(), ());
        }
    }
    let mut update = 0u64;
    let mut seen: HashMap<&str, ()> = HashMap::new();
    for chunks in new.files.values() {
        for (hash, len) in chunks {
            if !old_chunks.contains_key(hash.as_str()) && seen.insert(hash.as_str(), ()).is_none() {
                update += len;
            }
        }
    }
    update
}

// ---------------------------------------------------------------------------
// depot analyze-sharing
// ---------------------------------------------------------------------------

pub fn depot_analyze_sharing(
    workspace: &Path,
    app: Option<&str>,
    build: Option<&str>,
    out: Option<&Path>,
    json: bool,
) -> Result<()> {
    let ws = open(workspace)?;
    let app_id = ws.app_id(app)?;
    let build = match build {
        Some(id) => ws.build(&app_id, id)?,
        None => ws
            .builds(&app_id)?
            .into_iter()
            .last()
            .with_context(|| format!("app '{app_id}' has no builds yet"))?,
    };
    if build.depots.len() < 2 {
        bail!(
            "build '{}' has {} depot(s); sharing needs at least 2",
            build.id,
            build.depots.len()
        );
    }
    let mut indices = Vec::new();
    for d in &build.depots {
        indices.push(ws.depot_index(&app_id, &build.id, &d.depot_id)?);
    }
    let pairs = sharing::matrix(&indices);

    if json {
        println!("{}", serde_json::to_string_pretty(&pairs)?);
        return Ok(());
    }
    println!("depot sharing (build {}):", build.id);
    for p in &pairs {
        println!(
            "  {:<14} ↔ {:<14} shared {:>10}  unique {:>10} / {:>10}  reuse {:>5.1}%",
            p.depot_a,
            p.depot_b,
            human_bytes(p.shared_bytes),
            human_bytes(p.unique_a_bytes),
            human_bytes(p.unique_b_bytes),
            p.reuse_ratio * 100.0
        );
    }
    for p in &pairs {
        if p.reuse_ratio > 0.3 && p.shared_bytes > 1 << 20 {
            println!(
                "suggest  : '{}' and '{}' share {} — consider a shared depot so the \
                 content ships and stores once",
                p.depot_a,
                p.depot_b,
                human_bytes(p.shared_bytes)
            );
        }
    }
    if let Some(path) = out {
        let mut md = String::from("# Depot Sharing\n\n| Depot A | Depot B | Shared bytes | Unique A | Unique B | Reuse % |\n|---|---|---:|---:|---:|---:|\n");
        for p in &pairs {
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} | {:.1}% |\n",
                p.depot_a,
                p.depot_b,
                human_bytes(p.shared_bytes),
                human_bytes(p.unique_a_bytes),
                human_bytes(p.unique_b_bytes),
                p.reuse_ratio * 100.0
            ));
        }
        std::fs::write(path, md)?;
        eprintln!("report  : {}", path.display());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// install-plan
// ---------------------------------------------------------------------------

pub struct InstallPlanArgs<'a> {
    pub workspace: &'a Path,
    pub app: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub platform: Option<&'a str>,
    pub language: Option<&'a str>,
    /// Owned depot ids (csv already split). Empty = every non-optional depot.
    pub owned: Vec<String>,
    pub from_build: Option<&'a str>,
    pub to_build: Option<&'a str>,
    pub json: bool,
    pub out: Option<&'a Path>,
}

#[derive(Serialize)]
struct DepotPlan {
    depot: String,
    action: String, // no-op | update | install
    bytes: u64,
    route: String,
}

#[derive(Serialize)]
struct InstallPlan {
    app: String,
    branch: Option<String>,
    platform: Option<String>,
    language: Option<String>,
    owned: Vec<String>,
    from_build: Option<String>,
    to_build: String,
    depots: Vec<DepotPlan>,
    total_bytes: u64,
    note: String,
}

pub fn install_plan(args: &InstallPlanArgs) -> Result<()> {
    let ws = open(args.workspace)?;
    let app_id = ws.app_id(args.app)?;
    let app = ws.load_app(&app_id)?;

    let to_id = match (args.to_build, args.branch) {
        (Some(id), _) => id.to_string(),
        (None, Some(branch)) => app
            .branch(branch)?
            .current_build
            .clone()
            .with_context(|| format!("branch '{branch}' serves no build yet"))?,
        (None, None) => bail!("CAVS-E-INSTALL-PLAN-INVALID: pass --to or --branch"),
    };
    let to = ws.build(&app_id, &to_id)?;
    let from = match args.from_build {
        Some(id) => Some(ws.build(&app_id, id)?),
        None => None,
    };
    let platform = match args.platform {
        Some(p) => Some(
            Platform::parse(p)
                .ok_or_else(|| anyhow::anyhow!("unknown platform '{p}' (windows|linux|macos)"))?,
        ),
        None => None,
    };

    // Which depots does this client receive?
    let wanted: Vec<&cavs_workspace::DepotBuild> = to
        .depots
        .iter()
        .filter(|db| {
            let Ok(meta) = app.depot(&db.depot_id) else {
                return false;
            };
            // A depot that declares a platform/language is only delivered
            // when the client state matches it.
            let platform_ok = match (meta.platform, platform) {
                (None, _) => true,
                (Some(a), Some(b)) => a == b,
                (Some(_), None) => false,
            };
            let language_ok = match (&meta.language, args.language) {
                (None, _) => true,
                (Some(a), Some(b)) => a == b,
                (Some(_), None) => false,
            };
            let owned_ok = if args.owned.is_empty() {
                !meta.optional
            } else {
                args.owned.iter().any(|o| o == &db.depot_id) || !meta.optional
            };
            platform_ok && language_ok && owned_ok
        })
        .collect();
    if wanted.is_empty() {
        bail!(
            "CAVS-E-INSTALL-PLAN-INVALID: no depots match platform/language/ownership \
             in build '{}'",
            to.id
        );
    }

    // Indices the client already holds (from the installed build).
    let mut held: Vec<DepotIndex> = Vec::new();
    if let Some(from) = &from {
        for db in &from.depots {
            held.push(ws.depot_index(&app_id, &from.id, &db.depot_id)?);
        }
    }

    let mut plans: Vec<DepotPlan> = Vec::new();
    let mut total = 0u64;
    for db in &wanted {
        let new_idx = ws.depot_index(&app_id, &to.id, &db.depot_id)?;
        let had = from
            .as_ref()
            .map(|f| f.depots.iter().any(|d| d.depot_id == db.depot_id))
            .unwrap_or(false);
        let held_refs: Vec<&DepotIndex> = held.iter().collect();
        let bytes = sharing::fetch_bytes(&new_idx, &held_refs);
        // Chunks fetched for this depot are then locally available for
        // the depots that follow (cross-depot content sharing).
        held.push(new_idx.clone());
        let (action, route) = if bytes == 0 {
            ("no-op", "none — already up to date")
        } else if had {
            (
                "update",
                if bytes * 10 < new_idx.total_bytes {
                    "CAVS .cavsplan"
                } else {
                    "CAVS chunks / hybrid"
                },
            )
        } else {
            (
                "install",
                if bytes * 2 < new_idx.total_bytes {
                    "CAVS chunks (cross-depot reuse)"
                } else {
                    "bootstrap"
                },
            )
        };
        total += bytes;
        plans.push(DepotPlan {
            depot: db.depot_id.clone(),
            action: action.into(),
            bytes,
            route: route.into(),
        });
    }

    let plan = InstallPlan {
        app: app_id.clone(),
        branch: args.branch.map(String::from),
        platform: args.platform.map(String::from),
        language: args.language.map(String::from),
        owned: args.owned.clone(),
        from_build: from.as_ref().map(|f| f.id.clone()),
        to_build: to.id.clone(),
        depots: plans,
        total_bytes: total,
        note: "estimates from content indices (raw new chunk bytes, before wire \
               compression)"
            .into(),
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        print_install_plan(&plan);
    }
    if let Some(path) = args.out {
        std::fs::write(path, install_markdown(&plan))?;
        eprintln!("report  : {}", path.display());
    }
    Ok(())
}

fn print_install_plan(p: &InstallPlan) {
    println!("install-plan: app '{}' → {}", p.app, p.to_build);
    println!(
        "client  : platform {}, language {}, owned [{}], installed {}",
        p.platform.as_deref().unwrap_or("any"),
        p.language.as_deref().unwrap_or("any"),
        p.owned.join(", "),
        p.from_build.as_deref().unwrap_or("nothing")
    );
    for d in &p.depots {
        println!(
            "  {:<16} {:<8} {:>10}   {}",
            d.depot,
            d.action,
            human_bytes(d.bytes),
            d.route
        );
    }
    println!("total   : {}", human_bytes(p.total_bytes));
    println!("note    : {}", p.note);
}

fn install_markdown(p: &InstallPlan) -> String {
    let mut md = String::from("# Install Plan\n\n");
    md.push_str(&format!(
        "Player state:\n\n- platform: {}\n- language: {}\n- owned depots: {}\n- installed build: {}\n- target build: {}\n\n",
        p.platform.as_deref().unwrap_or("any"),
        p.language.as_deref().unwrap_or("any"),
        if p.owned.is_empty() { "(all required)".into() } else { p.owned.join(", ") },
        p.from_build.as_deref().unwrap_or("none"),
        p.to_build
    ));
    md.push_str("| Depot | Action | Download | Route |\n|---|---|---:|---|\n");
    for d in &p.depots {
        md.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            d.depot,
            d.action,
            human_bytes(d.bytes),
            d.route
        ));
    }
    md.push_str(&format!(
        "\nTotal:\n  **{}**\n\n> {}\n",
        human_bytes(p.total_bytes),
        p.note
    ));
    md
}
