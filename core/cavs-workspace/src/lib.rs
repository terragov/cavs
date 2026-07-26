//! Local app/depot/branch/build workspace: SteamPipe-like distribution
//! concepts (apps own depots, builds snapshot depots, branches point at
//! builds) modeled as local metadata only. No accounts, no remote
//! service, no platform — promotion and rollback are atomic file writes.
//!
//! ```text
//! cavs-workspace/
//!   cavs.toml                       # workspace: default app
//!   apps/<app>/app.toml             # depots + branches (+ current builds)
//!   apps/<app>/builds/<id>/build.toml
//!   apps/<app>/builds/<id>/<depot>.index.json   # content chunk index
//!   apps/<app>/reports/             # analyzer/preview output
//! ```

pub mod sharing;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Error-code prefixes used in messages (searchable, stable).
pub const E_CORRUPT: &str = "CAVS-E-WORKSPACE-CORRUPT";
pub const E_DEPOT: &str = "CAVS-E-DEPOT-NOT-FOUND";
pub const E_BRANCH: &str = "CAVS-E-BRANCH-NOT-FOUND";
pub const E_BUILD: &str = "CAVS-E-BUILD-NOT-FOUND";

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Windows,
    Linux,
    Macos,
}

impl Platform {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "windows" | "win" => Some(Platform::Windows),
            "linux" => Some(Platform::Linux),
            "macos" | "mac" | "osx" => Some(Platform::Macos),
            _ => None,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Platform::Windows => "windows",
            Platform::Linux => "linux",
            Platform::Macos => "macos",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum BranchVisibility {
    #[default]
    Public,
    Private,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Depot {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default)]
    pub optional: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Branch {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub visibility: BranchVisibility,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_build: Option<String>,
    /// Every build this branch has pointed at, oldest first (promotion
    /// history; rollback re-points to an earlier entry).
    #[serde(default)]
    pub history: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct App {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub depots: Vec<Depot>,
    #[serde(default)]
    pub branches: Vec<Branch>,
}

impl App {
    pub fn depot(&self, id: &str) -> Result<&Depot> {
        self.depots
            .iter()
            .find(|d| d.id == id)
            .with_context(|| format!("{E_DEPOT}: depot '{id}' not found in app '{}'", self.id))
    }
    pub fn branch(&self, id: &str) -> Result<&Branch> {
        self.branches
            .iter()
            .find(|b| b.id == id)
            .with_context(|| format!("{E_BRANCH}: branch '{id}' not found in app '{}'", self.id))
    }
    pub fn branch_mut(&mut self, id: &str) -> Result<&mut Branch> {
        let app_id = self.id.clone();
        self.branches
            .iter_mut()
            .find(|b| b.id == id)
            .with_context(|| format!("{E_BRANCH}: branch '{id}' not found in app '{app_id}'"))
    }
}

/// One depot's content inside a build.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DepotBuild {
    pub depot_id: String,
    /// The source directory the content was indexed from.
    pub source_path: String,
    pub total_bytes: u64,
    pub files: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Build {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub created_at: u64,
    #[serde(default)]
    pub depots: Vec<DepotBuild>,
}

/// Content index of one depot in one build: enough to compute sharing,
/// update estimates and install plans without re-reading the source tree.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DepotIndex {
    pub depot_id: String,
    pub total_bytes: u64,
    /// path → ordered (chunk-hash hex, len) list.
    pub files: BTreeMap<String, Vec<(String, u64)>>,
}

const CDC: cavs_chunker::ChunkMode = cavs_chunker::ChunkMode::Cdc {
    min: 16 * 1024,
    avg: 64 * 1024,
    max: 256 * 1024,
    norm: cavs_chunker::NORM_DEFAULT,
};

impl DepotIndex {
    /// Index a directory (or a single artifact) with the CAVS asset
    /// chunker.
    pub fn scan(depot_id: &str, root: &Path) -> Result<DepotIndex> {
        let mut files = BTreeMap::new();
        let mut total = 0u64;
        for (rel, abs) in walk(root)? {
            let bytes = std::fs::read(&abs)?;
            total += bytes.len() as u64;
            let chunks = cavs_chunker::split(&bytes, CDC)
                .into_iter()
                .map(|r| {
                    (
                        cavs_hash::to_hex(&cavs_hash::hash_chunk(&bytes[r.clone()])),
                        r.len() as u64,
                    )
                })
                .collect();
            files.insert(rel, chunks);
        }
        Ok(DepotIndex {
            depot_id: depot_id.into(),
            total_bytes: total,
            files,
        })
    }
}

fn walk(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    if root.is_file() {
        let name = root
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "artifact".into());
        return Ok(vec![(name, root.to_path_buf())]);
    }
    let mut out = Vec::new();
    fn rec(base: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                rec(base, &path, out)?;
            } else if ft.is_file() {
                out.push((
                    path.strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                    path,
                ));
            }
        }
        Ok(())
    }
    rec(root, root, &mut out)?;
    out.sort();
    Ok(out)
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct WorkspaceConfig {
    version: u32,
    default_app: String,
}

/// Handle to an on-disk workspace.
#[derive(Debug)]
pub struct Workspace {
    pub root: PathBuf,
}

impl Workspace {
    /// Create a workspace with one app. Fails if `cavs.toml` already
    /// exists there.
    pub fn init(root: &Path, app_id: &str) -> Result<Workspace> {
        let config = root.join("cavs.toml");
        if config.exists() {
            bail!("{} already contains a workspace", root.display());
        }
        validate_id(app_id)?;
        std::fs::create_dir_all(root.join("apps").join(app_id).join("builds"))?;
        std::fs::create_dir_all(root.join("apps").join(app_id).join("reports"))?;
        let ws = Workspace {
            root: root.to_path_buf(),
        };
        write_atomic(
            &config,
            toml::to_string_pretty(&WorkspaceConfig {
                version: 1,
                default_app: app_id.into(),
            })?
            .as_bytes(),
        )?;
        ws.save_app(&App {
            id: app_id.into(),
            name: app_id.into(),
            depots: Vec::new(),
            branches: Vec::new(),
        })?;
        Ok(ws)
    }

    /// Open an existing workspace.
    pub fn open(root: &Path) -> Result<Workspace> {
        if !root.join("cavs.toml").is_file() {
            bail!(
                "{E_CORRUPT}: {} is not a CAVS workspace (no cavs.toml)",
                root.display()
            );
        }
        Ok(Workspace {
            root: root.to_path_buf(),
        })
    }

    fn config(&self) -> Result<WorkspaceConfig> {
        let raw = std::fs::read_to_string(self.root.join("cavs.toml"))?;
        toml::from_str(&raw).with_context(|| format!("{E_CORRUPT}: cavs.toml is not valid"))
    }

    /// The app to operate on: the given id, or the workspace default.
    pub fn app_id(&self, requested: Option<&str>) -> Result<String> {
        match requested {
            Some(id) => Ok(id.to_string()),
            None => Ok(self.config()?.default_app),
        }
    }

    fn app_dir(&self, app_id: &str) -> PathBuf {
        self.root.join("apps").join(app_id)
    }

    pub fn load_app(&self, app_id: &str) -> Result<App> {
        let path = self.app_dir(app_id).join("app.toml");
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("{E_CORRUPT}: app '{app_id}' has no app.toml"))?;
        toml::from_str(&raw)
            .with_context(|| format!("{E_CORRUPT}: {} is not valid", path.display()))
    }

    pub fn save_app(&self, app: &App) -> Result<()> {
        let dir = self.app_dir(&app.id);
        std::fs::create_dir_all(dir.join("builds"))?;
        write_atomic(
            &dir.join("app.toml"),
            toml::to_string_pretty(app)?.as_bytes(),
        )
    }

    pub fn add_depot(&self, app_id: &str, depot: Depot) -> Result<()> {
        validate_id(&depot.id)?;
        let mut app = self.load_app(app_id)?;
        if app.depots.iter().any(|d| d.id == depot.id) {
            bail!("depot '{}' already exists in app '{app_id}'", depot.id);
        }
        app.depots.push(depot);
        self.save_app(&app)
    }

    pub fn add_branch(&self, app_id: &str, branch: Branch) -> Result<()> {
        validate_id(&branch.id)?;
        let mut app = self.load_app(app_id)?;
        if app.branches.iter().any(|b| b.id == branch.id) {
            bail!("branch '{}' already exists in app '{app_id}'", branch.id);
        }
        app.branches.push(branch);
        self.save_app(&app)
    }

    /// Record a build: index every depot source and persist metadata.
    pub fn create_build(
        &self,
        app_id: &str,
        label: Option<&str>,
        branch: Option<&str>,
        depot_sources: &[(String, PathBuf)],
        created_at: u64,
    ) -> Result<Build> {
        let mut app = self.load_app(app_id)?;
        if depot_sources.is_empty() {
            bail!("a build needs at least one --depot id=path");
        }
        for (depot_id, _) in depot_sources {
            app.depot(depot_id)?;
        }
        let id = next_build_id(&self.builds(app_id)?);
        let build_dir = self.app_dir(app_id).join("builds").join(&id);
        std::fs::create_dir_all(&build_dir)?;

        let mut depots = Vec::new();
        for (depot_id, source) in depot_sources {
            let index = DepotIndex::scan(depot_id, source)?;
            write_atomic(
                &build_dir.join(format!("{depot_id}.index.json")),
                &serde_json::to_vec(&index)?,
            )?;
            depots.push(DepotBuild {
                depot_id: depot_id.clone(),
                source_path: source.display().to_string(),
                total_bytes: index.total_bytes,
                files: index.files.len(),
            });
        }
        let build = Build {
            id: id.clone(),
            label: label.map(String::from),
            created_at,
            depots,
        };
        write_atomic(
            &build_dir.join("build.toml"),
            toml::to_string_pretty(&build)?.as_bytes(),
        )?;

        if let Some(branch_id) = branch {
            let b = app.branch_mut(branch_id)?;
            b.current_build = Some(id.clone());
            b.history.push(id);
            self.save_app(&app)?;
        }
        Ok(build)
    }

    pub fn builds(&self, app_id: &str) -> Result<Vec<Build>> {
        let dir = self.app_dir(app_id).join("builds");
        let mut out = Vec::new();
        if !dir.is_dir() {
            return Ok(out);
        }
        for entry in std::fs::read_dir(&dir)? {
            let meta = entry?.path().join("build.toml");
            if meta.is_file() {
                let raw = std::fs::read_to_string(&meta)?;
                out.push(
                    toml::from_str(&raw)
                        .with_context(|| format!("{E_CORRUPT}: {} is not valid", meta.display()))?,
                );
            }
        }
        out.sort_by(|a: &Build, b: &Build| a.id.cmp(&b.id));
        Ok(out)
    }

    pub fn build(&self, app_id: &str, build_id: &str) -> Result<Build> {
        let meta = self
            .app_dir(app_id)
            .join("builds")
            .join(build_id)
            .join("build.toml");
        if !meta.is_file() {
            bail!("{E_BUILD}: build '{build_id}' not found in app '{app_id}'");
        }
        let raw = std::fs::read_to_string(&meta)?;
        toml::from_str(&raw)
            .with_context(|| format!("{E_CORRUPT}: {} is not valid", meta.display()))
    }

    pub fn depot_index(&self, app_id: &str, build_id: &str, depot_id: &str) -> Result<DepotIndex> {
        let path = self
            .app_dir(app_id)
            .join("builds")
            .join(build_id)
            .join(format!("{depot_id}.index.json"));
        if !path.is_file() {
            bail!("{E_DEPOT}: build '{build_id}' has no depot '{depot_id}'");
        }
        serde_json::from_slice(&std::fs::read(&path)?)
            .with_context(|| format!("{E_CORRUPT}: {} is not valid", path.display()))
    }

    /// Point `branch` at `build`. Atomic: the app.toml swap is a rename.
    pub fn promote(&self, app_id: &str, branch_id: &str, build_id: &str) -> Result<()> {
        self.build(app_id, build_id)?; // must exist
        let mut app = self.load_app(app_id)?;
        let b = app.branch_mut(branch_id)?;
        b.current_build = Some(build_id.to_string());
        b.history.push(build_id.to_string());
        self.save_app(&app)
    }

    /// Re-point `branch` at an earlier build it has served before.
    pub fn rollback(&self, app_id: &str, branch_id: &str, to_build: &str) -> Result<()> {
        let app = self.load_app(app_id)?;
        let b = app.branch(branch_id)?;
        if !b.history.iter().any(|h| h == to_build) {
            bail!(
                "{E_BUILD}: branch '{branch_id}' never served build '{to_build}' \
                 (history: {})",
                b.history.join(", ")
            );
        }
        self.promote(app_id, branch_id, to_build)
    }

    pub fn reports_dir(&self, app_id: &str) -> PathBuf {
        self.app_dir(app_id).join("reports")
    }
}

/// build_1001, build_1002, ... — stable, sortable ids.
fn next_build_id(existing: &[Build]) -> String {
    let max = existing
        .iter()
        .filter_map(|b| b.id.strip_prefix("build_")?.parse::<u64>().ok())
        .max()
        .unwrap_or(1000);
    format!("build_{}", max + 1)
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("'{id}' is not a valid id (use letters, digits, '-', '_')");
    }
    Ok(())
}

/// Write via temp file + rename so readers never see a partial file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn depot(id: &str) -> Depot {
        Depot {
            id: id.into(),
            name: id.into(),
            platform: None,
            language: None,
            optional: false,
        }
    }

    fn branch(id: &str) -> Branch {
        Branch {
            id: id.into(),
            name: id.into(),
            visibility: BranchVisibility::Public,
            current_build: None,
            history: Vec::new(),
        }
    }

    #[test]
    fn init_add_build_promote_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        let ws = Workspace::init(&root, "my-app").unwrap();
        ws.add_depot("my-app", depot("base")).unwrap();
        ws.add_depot("my-app", depot("windows")).unwrap();
        ws.add_branch("my-app", branch("public")).unwrap();
        ws.add_branch("my-app", branch("beta")).unwrap();

        let src = dir.path().join("content");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.bin"), vec![1u8; 100_000]).unwrap();

        let b1 = ws
            .create_build(
                "my-app",
                Some("v1"),
                Some("beta"),
                &[("base".into(), src.clone())],
                1,
            )
            .unwrap();
        assert_eq!(b1.id, "build_1001");
        let b2 = ws
            .create_build(
                "my-app",
                Some("v2"),
                Some("beta"),
                &[("base".into(), src)],
                2,
            )
            .unwrap();
        assert_eq!(b2.id, "build_1002");

        // beta followed both builds; public has none yet.
        let app = ws.load_app("my-app").unwrap();
        assert_eq!(
            app.branch("beta").unwrap().current_build.as_deref(),
            Some("build_1002")
        );
        assert_eq!(app.branch("public").unwrap().current_build, None);

        ws.promote("my-app", "public", "build_1002").unwrap();
        let app = ws.load_app("my-app").unwrap();
        assert_eq!(
            app.branch("public").unwrap().current_build.as_deref(),
            Some("build_1002")
        );

        // Rollback only to builds the branch has served.
        assert!(ws.rollback("my-app", "public", "build_1001").is_err());
        ws.rollback("my-app", "beta", "build_1001").unwrap();
        let app = ws.load_app("my-app").unwrap();
        assert_eq!(
            app.branch("beta").unwrap().current_build.as_deref(),
            Some("build_1001")
        );
    }

    #[test]
    fn missing_entities_use_error_codes() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init(&dir.path().join("ws"), "g").unwrap();
        let err = ws.build("g", "build_9").unwrap_err().to_string();
        assert!(err.contains("CAVS-E-BUILD-NOT-FOUND"), "{err}");
        let app = ws.load_app("g").unwrap();
        let err = app.branch("nope").unwrap_err().to_string();
        assert!(err.contains("CAVS-E-BRANCH-NOT-FOUND"), "{err}");
        let err = app.depot("nope").unwrap_err().to_string();
        assert!(err.contains("CAVS-E-DEPOT-NOT-FOUND"), "{err}");

        let err = Workspace::open(&dir.path().join("empty"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("CAVS-E-WORKSPACE-CORRUPT"), "{err}");
    }

    #[test]
    fn build_needs_known_depots() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init(&dir.path().join("ws"), "g").unwrap();
        let src = dir.path().join("c");
        std::fs::create_dir_all(&src).unwrap();
        let err = ws
            .create_build("g", None, None, &[("ghost".into(), src)], 1)
            .unwrap_err()
            .to_string();
        assert!(err.contains("CAVS-E-DEPOT-NOT-FOUND"), "{err}");
    }

    #[test]
    fn depot_index_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::init(&dir.path().join("ws"), "g").unwrap();
        ws.add_depot("g", depot("base")).unwrap();
        let src = dir.path().join("c");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.bin"), vec![7u8; 200_000]).unwrap();
        std::fs::write(src.join("sub/b.bin"), vec![9u8; 50_000]).unwrap();
        let b = ws
            .create_build("g", None, None, &[("base".into(), src)], 1)
            .unwrap();
        let idx = ws.depot_index("g", &b.id, "base").unwrap();
        assert_eq!(idx.total_bytes, 250_000);
        assert_eq!(idx.files.len(), 2);
        assert!(idx.files.contains_key("sub/b.bin"));
    }
}
