//! `cavs serve` (v0.9.0): a local content server over a CAVS workspace,
//! for development and plugin testing only — SteamPipe's "local content
//! server" idea without any production ambitions. No auth, plain HTTP,
//! and it says so at startup. Production delivery stays with
//! `cavs-server`.

// Handlers return `Result<T, Response>` so errors short-circuit into an
// HTTP response; the Response-sized Err is intentional for a dev server.
#![allow(clippy::result_large_err)]

use anyhow::{Context, Result};
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

pub struct ServeArgs {
    pub workspace: PathBuf,
    pub app: Option<String>,
    pub branch: Option<String>,
    /// Optional directory of published release files, served under
    /// /api/assets/{asset}/{file} (e.g. `cavs publish-dir` output).
    pub releases: Option<PathBuf>,
    pub port: u16,
}

struct ServeState {
    root: PathBuf,
    default_app: Option<String>,
    releases: Option<PathBuf>,
}

type Shared = Arc<ServeState>;

fn ws(state: &ServeState) -> Result<cavs_workspace::Workspace, Response> {
    cavs_workspace::Workspace::open(&state.root).map_err(|e| err(StatusCode::NOT_FOUND, e))
}

fn err(code: StatusCode, e: impl std::fmt::Display) -> Response {
    (code, format!("{e}")).into_response()
}

pub fn serve(args: &ServeArgs) -> Result<()> {
    // Validate the workspace before starting the runtime.
    let workspace = cavs_workspace::Workspace::open(&args.workspace)?;
    let default_app = Some(workspace.app_id(args.app.as_deref())?);

    let state: Shared = Arc::new(ServeState {
        root: args.workspace.clone(),
        default_app,
        releases: args.releases.clone(),
    });

    let app = Router::new()
        .route("/", get(index))
        .route(
            "/api/apps/{app}/branches/{branch}/current",
            get(branch_current),
        )
        .route("/api/apps/{app}/builds/{build}", get(build_meta))
        .route(
            "/api/apps/{app}/builds/{build}/depots/{depot}/index",
            get(depot_index),
        )
        .route(
            "/api/apps/{app}/builds/{build}/depots/{depot}/files/{*path}",
            get(depot_file),
        )
        .route(
            "/api/apps/{app}/builds/{build}/depots/{depot}/chunks/{hash}",
            get(depot_chunk),
        )
        .route("/api/assets/{asset}/{file}", get(release_file))
        .route("/api/preview", get(preview))
        .with_state(state);

    eprintln!("+----------------------------------------------------------------+");
    eprintln!("|  WARNING: DEVELOPMENT SERVER ONLY                              |");
    eprintln!("|  No auth, no TLS, not production hardened.                     |");
    eprintln!("|  Never expose this to the internet or real end users.          |");
    eprintln!("|  Use cavs-server for real deployments.                         |");
    eprintln!("+----------------------------------------------------------------+");
    if let Some(branch) = &args.branch {
        eprintln!("[serve] default branch: {branch}");
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", args.port))
            .await
            .with_context(|| format!("cannot bind 127.0.0.1:{}", args.port))?;
        println!("listening on http://{}", listener.local_addr()?);
        axum::serve(listener, app).await?;
        Ok(())
    })
}

async fn index(State(state): State<Shared>) -> Response {
    let app = state.default_app.clone().unwrap_or_default();
    (
        StatusCode::OK,
        format!(
            "cavs serve — DEVELOPMENT SERVER ONLY (no auth, no TLS, not production hardened)\n\
             workspace: {}\n\
             default app: {app}\n\n\
             endpoints:\n\
             GET /api/apps/{{app}}/branches/{{branch}}/current\n\
             GET /api/apps/{{app}}/builds/{{build}}\n\
             GET /api/apps/{{app}}/builds/{{build}}/depots/{{depot}}/index\n\
             GET /api/apps/{{app}}/builds/{{build}}/depots/{{depot}}/files/{{path}} (Range supported)\n\
             GET /api/apps/{{app}}/builds/{{build}}/depots/{{depot}}/chunks/{{hash}}\n\
             GET /api/assets/{{asset}}/{{file}} (published release files)\n\
             GET /api/preview?app=...&from=build_...&to=build_...\n",
            state.root.display()
        ),
    )
        .into_response()
}

async fn branch_current(
    State(state): State<Shared>,
    AxPath((app, branch)): AxPath<(String, String)>,
) -> Response {
    let ws = match ws(&state) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let meta = match ws.load_app(&app) {
        Ok(a) => a,
        Err(e) => return err(StatusCode::NOT_FOUND, e),
    };
    let b = match meta.branch(&branch) {
        Ok(b) => b.clone(),
        Err(e) => return err(StatusCode::NOT_FOUND, e),
    };
    let build = match &b.current_build {
        Some(id) => match ws.build(&app, id) {
            Ok(build) => Some(build),
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
        },
        None => None,
    };
    Json(serde_json::json!({ "branch": b, "build": build })).into_response()
}

async fn build_meta(
    State(state): State<Shared>,
    AxPath((app, build)): AxPath<(String, String)>,
) -> Response {
    let ws = match ws(&state) {
        Ok(w) => w,
        Err(r) => return r,
    };
    match ws.build(&app, &build) {
        Ok(b) => Json(b).into_response(),
        Err(e) => err(StatusCode::NOT_FOUND, e),
    }
}

async fn depot_index(
    State(state): State<Shared>,
    AxPath((app, build, depot)): AxPath<(String, String, String)>,
) -> Response {
    let ws = match ws(&state) {
        Ok(w) => w,
        Err(r) => return r,
    };
    match ws.depot_index(&app, &build, &depot) {
        Ok(idx) => Json(idx).into_response(),
        Err(e) => err(StatusCode::NOT_FOUND, e),
    }
}

/// The on-disk source directory a depot was indexed from.
fn depot_source(
    ws: &cavs_workspace::Workspace,
    app: &str,
    build: &str,
    depot: &str,
) -> Result<PathBuf, Response> {
    let b = ws
        .build(app, build)
        .map_err(|e| err(StatusCode::NOT_FOUND, e))?;
    let db = b
        .depots
        .iter()
        .find(|d| d.depot_id == depot)
        .ok_or_else(|| {
            err(
                StatusCode::NOT_FOUND,
                format!("no depot '{depot}' in {build}"),
            )
        })?;
    let root = PathBuf::from(&db.source_path);
    if !root.exists() {
        return Err(err(
            StatusCode::GONE,
            format!("depot source {} no longer exists", root.display()),
        ));
    }
    Ok(root)
}

/// Resolve a request path inside a depot root, rejecting traversal.
fn safe_join(root: &std::path::Path, rel: &str) -> Result<PathBuf, Response> {
    let bad = rel.starts_with('/')
        || rel.contains('\\')
        || rel.split('/').any(|seg| seg == ".." || seg.is_empty());
    if bad {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "CAVS-E-ANALYZE-PATH-TRAVERSAL",
        ));
    }
    Ok(root.join(rel))
}

fn range_of(headers: &HeaderMap, len: u64) -> Option<(u64, u64)> {
    let value = headers.get(header::RANGE)?.to_str().ok()?;
    let spec = value.strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    let start: u64 = start.parse().ok()?;
    let end: u64 = if end.is_empty() {
        len.saturating_sub(1)
    } else {
        end.parse().ok()?
    };
    (start <= end && end < len).then_some((start, end))
}

fn serve_bytes(headers: &HeaderMap, bytes: Vec<u8>) -> Response {
    let len = bytes.len() as u64;
    match range_of(headers, len) {
        Some((start, end)) => {
            let body = bytes[start as usize..=end as usize].to_vec();
            (
                StatusCode::PARTIAL_CONTENT,
                [
                    (header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}")),
                    (header::ACCEPT_RANGES, "bytes".into()),
                ],
                body,
            )
                .into_response()
        }
        None => (
            StatusCode::OK,
            [(header::ACCEPT_RANGES, "bytes".to_string())],
            bytes,
        )
            .into_response(),
    }
}

async fn depot_file(
    State(state): State<Shared>,
    AxPath((app, build, depot, path)): AxPath<(String, String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let ws = match ws(&state) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let root = match depot_source(&ws, &app, &build, &depot) {
        Ok(r) => r,
        Err(r) => return r,
    };
    let file = match safe_join(&root, &path) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match std::fs::read(&file) {
        Ok(bytes) => serve_bytes(&headers, bytes),
        Err(e) => err(StatusCode::NOT_FOUND, e),
    }
}

/// Serve one chunk by hash: located via the depot index (cumulative
/// offsets per file), read from the depot source.
async fn depot_chunk(
    State(state): State<Shared>,
    AxPath((app, build, depot, hash)): AxPath<(String, String, String, String)>,
) -> Response {
    let ws = match ws(&state) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let idx = match ws.depot_index(&app, &build, &depot) {
        Ok(i) => i,
        Err(e) => return err(StatusCode::NOT_FOUND, e),
    };
    let root = match depot_source(&ws, &app, &build, &depot) {
        Ok(r) => r,
        Err(r) => return r,
    };
    for (rel, chunks) in &idx.files {
        let mut offset = 0u64;
        for (h, len) in chunks {
            if h == &hash {
                let file = match safe_join(&root, rel) {
                    Ok(f) => f,
                    Err(r) => return r,
                };
                use std::io::{Read, Seek, SeekFrom};
                let mut f = match std::fs::File::open(&file) {
                    Ok(f) => f,
                    Err(e) => return err(StatusCode::GONE, e),
                };
                let mut buf = vec![0u8; *len as usize];
                if f.seek(SeekFrom::Start(offset)).is_err() || f.read_exact(&mut buf).is_err() {
                    return err(StatusCode::GONE, "chunk out of range (source changed?)");
                }
                // Integrity: the source may have changed since indexing.
                if cavs_hash::to_hex(&cavs_hash::hash_chunk(&buf)) != hash {
                    return err(
                        StatusCode::GONE,
                        "CAVS-E-CHUNK-HASH-MISMATCH: source changed since indexing",
                    );
                }
                return (StatusCode::OK, buf).into_response();
            }
            offset += len;
        }
    }
    err(
        StatusCode::NOT_FOUND,
        format!("chunk {hash} not in depot '{depot}'"),
    )
}

/// Published release files (e.g. `cavs publish-dir` output):
/// /api/assets/{asset}/{file} → <releases>/<asset>/<file>.
async fn release_file(
    State(state): State<Shared>,
    AxPath((asset, file)): AxPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let Some(releases) = &state.releases else {
        return err(
            StatusCode::NOT_FOUND,
            "no --releases directory configured; run with --releases <dir>",
        );
    };
    let root = releases.join(&asset);
    let path = match safe_join(&root, &file) {
        Ok(p) => p,
        Err(r) => return r,
    };
    match std::fs::read(&path) {
        Ok(bytes) => serve_bytes(&headers, bytes),
        Err(e) => err(StatusCode::NOT_FOUND, e),
    }
}

#[derive(Deserialize)]
struct PreviewParams {
    app: Option<String>,
    from: String,
    to: String,
}

/// Per-depot update estimate between two builds (raw new chunk bytes).
async fn preview(State(state): State<Shared>, Query(params): Query<PreviewParams>) -> Response {
    let ws = match ws(&state) {
        Ok(w) => w,
        Err(r) => return r,
    };
    let app = params
        .app
        .or_else(|| state.default_app.clone())
        .unwrap_or_default();
    let from = match ws.build(&app, &params.from) {
        Ok(b) => b,
        Err(e) => return err(StatusCode::NOT_FOUND, e),
    };
    let to = match ws.build(&app, &params.to) {
        Ok(b) => b,
        Err(e) => return err(StatusCode::NOT_FOUND, e),
    };
    let mut per_depot: HashMap<String, u64> = HashMap::new();
    for db in &to.depots {
        let new_idx = match ws.depot_index(&app, &to.id, &db.depot_id) {
            Ok(i) => i,
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
        };
        let bytes = if from.depots.iter().any(|d| d.depot_id == db.depot_id) {
            let old_idx = match ws.depot_index(&app, &from.id, &db.depot_id) {
                Ok(i) => i,
                Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
            };
            cavs_workspace::sharing::fetch_bytes(&new_idx, &[&old_idx])
        } else {
            new_idx.total_bytes
        };
        per_depot.insert(db.depot_id.clone(), bytes);
    }
    let total: u64 = per_depot.values().sum();
    Json(serde_json::json!({
        "app": app,
        "from": from.id,
        "to": to.id,
        "estimated_update_bytes_per_depot": per_depot,
        "estimated_update_bytes_total": total,
        "note": "raw new chunk bytes from content indices, before wire compression",
    }))
    .into_response()
}
