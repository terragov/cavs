//! `cavs-client` — native CAVS-1 streaming client with a persistent
//! content-addressable cache.
//!
//! On fetch it announces its have-set to the origin, receives inline/ref
//! plans, resolves references from the local cache, verifies every chunk by
//! BLAKE3, reconstructs playable outputs and reports real egress savings.

// The retry closures deliberately return `ureq::Error` (272 bytes) so the
// backoff policy can classify transient vs permanent failures; the cost of
// the large Err variant on these cold paths is irrelevant.
#![allow(clippy::result_large_err)]

mod cache;
mod hybrid;
mod journal;
mod parallel;
mod retry;
mod serverless;

use anyhow::{anyhow, bail, Context, Result};
use cache::ChunkCache;
use cavs_hash::to_hex;
use cavs_proto::errors::ErrorCode;
use cavs_proto::{BatchRequest, DeliveryInstr, Manifest, SessionOpenRequest, SessionOpenResponse};
use clap::{Parser, Subcommand};
use hybrid::HybridOpts;
use journal::{ResumeJournal, ResumeState};
use std::path::{Path, PathBuf};

/// Segments requested per batch round-trip.
const BATCH_SIZE: usize = 64;
/// Above this many cached chunks, summarise the have-set with a Bloom filter
/// instead of an exact hash list (keeps the session-open body compact).
const BLOOM_THRESHOLD: usize = 256;

#[derive(Parser)]
#[command(name = "cavs-client", version, about = "CAVS-1 streaming client")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List assets available on a server.
    List {
        server: String,
        /// Trust this PEM certificate (e.g. a self-signed dev cert).
        #[arg(long)]
        ca: Option<PathBuf>,
    },
    /// Fetch an asset, reconstruct it under -o, and print egress stats.
    Fetch {
        /// Server base URL, e.g. http://127.0.0.1:8990
        server: String,
        /// Asset name as listed by the server.
        asset: String,
        /// Output directory.
        #[arg(short, long)]
        output: PathBuf,
        /// Persistent chunk cache directory (survives across fetches).
        #[arg(long, default_value = ".cavs-cache")]
        cache: PathBuf,
        /// Write exact fetch statistics as JSON to this path.
        #[arg(long)]
        stats_json: Option<PathBuf>,
        /// Trust this PEM certificate (e.g. a self-signed dev cert).
        #[arg(long)]
        ca: Option<PathBuf>,
        /// Require the asset to be signed by this Ed25519 public key
        /// (64 hex chars, or a path to a .pub file).
        #[arg(long)]
        pubkey: Option<String>,
        /// Start clean instead of resuming a previous interrupted fetch.
        #[arg(long)]
        no_resume: bool,
        /// Previously installed artifact (e.g. the old game_v1.pck): verified
        /// byte ranges are copied from it instead of fetched (v0.6.0 hybrid
        /// reconstruction).
        #[arg(long)]
        previous_artifact: Option<PathBuf>,
        /// Disable hybrid planning: v0.5 behaviour (cache + network only).
        #[arg(long)]
        no_hybrid: bool,
        /// Write the reconstruction plan(s) as JSON to this path.
        #[arg(long)]
        dump_plan: Option<PathBuf>,
        /// Disable no-op detection: always reconstruct outputs even when
        /// they already match the target hashes.
        #[arg(long)]
        force_reconstruct: bool,
        /// Directory assets: after a successful apply, remove files that are
        /// no longer part of the container (off by default; unknown files —
        /// e.g. mods — are preserved).
        #[arg(long)]
        prune: bool,
        /// Enable the content-addressed parallel chunk fetch (container
        /// payloads): download missing chunks concurrently by hash from the
        /// immutable, edge-cacheable chunk endpoint, using this many
        /// connections. Best when a CDN fronts the origin (each chunk is an
        /// edge-cached object). 0 (default) keeps the session/batch path,
        /// which preserves the origin's packfile read-coalescing.
        #[arg(long, default_value_t = 0)]
        connections: usize,
    },
    /// Fetch an asset from a *static* export (no cavs-server): an object
    /// tree produced by `cavs store export --static-plans`, hosted on S3 /
    /// R2 / GitHub Pages / nginx / a local folder. The client plans the
    /// missing set locally and downloads only the chunks it lacks via HTTP
    /// Range requests, verified end to end.
    FetchStatic {
        /// Base URL or local directory of the static export
        /// (e.g. https://cdn.example.com/dist or ./dist).
        base: String,
        /// Asset name (the `<name>` under `assets/<name>/` in the export).
        asset: String,
        /// Output directory.
        #[arg(short, long)]
        output: PathBuf,
        /// Persistent chunk cache directory (survives across fetches).
        #[arg(long, default_value = ".cavs-cache")]
        cache: PathBuf,
        /// Concurrent range requests.
        #[arg(long, default_value_t = parallel::DEFAULT_CONNECTIONS)]
        connections: usize,
        /// Trust this PEM certificate for an HTTPS base (dev self-signed).
        #[arg(long)]
        ca: Option<PathBuf>,
        /// Require the asset to be signed by this Ed25519 public key.
        #[arg(long)]
        pubkey: Option<String>,
    },
    /// Resume interrupted fetches recorded in the cache's journal.
    Resume {
        /// Persistent chunk cache directory holding the journal.
        #[arg(long, default_value = ".cavs-cache")]
        cache: PathBuf,
        /// Resume only this asset (default: every pending journal).
        #[arg(long)]
        asset: Option<String>,
        /// Trust this PEM certificate (e.g. a self-signed dev cert).
        #[arg(long)]
        ca: Option<PathBuf>,
        /// Require assets to be signed by this Ed25519 public key.
        #[arg(long)]
        pubkey: Option<String>,
    },
    /// Verify, repair or garbage-collect the persistent chunk cache.
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
    /// Fetch to a temp dir and play the first video track with ffplay.
    Play {
        server: String,
        asset: String,
        #[arg(long, default_value = ".cavs-cache")]
        cache: PathBuf,
        #[arg(long)]
        ca: Option<PathBuf>,
        #[arg(long)]
        pubkey: Option<String>,
    },
}

#[derive(Subcommand)]
enum CacheAction {
    /// Re-hash every cached chunk (v0.5.0). Corrupt entries move to
    /// `<cache>/quarantine/` (or are deleted with --delete); stray temp
    /// files are removed. The cache heals itself: quarantined chunks are
    /// simply re-fetched by the next update.
    Verify {
        #[arg(long, default_value = ".cavs-cache")]
        cache: PathBuf,
        /// Delete corrupt entries instead of quarantining them.
        #[arg(long)]
        delete: bool,
    },
    /// Re-fetch an asset's missing or corrupt chunks from a server, so the
    /// next update starts from a fully valid cache.
    Repair {
        /// Server base URL, e.g. http://127.0.0.1:8990
        server: String,
        /// Asset name as listed by the server.
        asset: String,
        #[arg(long, default_value = ".cavs-cache")]
        cache: PathBuf,
        /// Trust this PEM certificate (e.g. a self-signed dev cert).
        #[arg(long)]
        ca: Option<PathBuf>,
    },
    /// Evict least-recently-used chunks until the cache fits --max-size.
    Gc {
        #[arg(long, default_value = ".cavs-cache")]
        cache: PathBuf,
        /// Size budget, e.g. 10GiB, 500MiB or plain bytes.
        #[arg(long)]
        max_size: String,
    },
}

fn main() -> Result<()> {
    // Pick a rustls crypto provider explicitly (see cavs-server note).
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cli = Cli::parse();
    match cli.command {
        Command::List { server, ca } => {
            let agent = build_agent(ca.as_deref())?;
            let body = http_get_string(&agent, &format!("{server}/api/assets"))?;
            let assets: Vec<cavs_proto::AssetSummary> = serde_json::from_str(&body)?;
            for a in assets {
                println!(
                    "{}  tracks={} segments={} chunks={}",
                    a.name, a.tracks, a.segments, a.unique_chunks
                );
            }
            Ok(())
        }
        Command::Fetch {
            server,
            asset,
            output,
            cache,
            stats_json,
            ca,
            pubkey,
            no_resume,
            previous_artifact,
            no_hybrid,
            dump_plan,
            force_reconstruct,
            prune,
            connections,
        } => {
            let agent = build_agent(ca.as_deref())?;
            let hybrid_opts = HybridOpts {
                previous_artifact,
                enabled: !no_hybrid,
                dump_plan,
                force_reconstruct,
                prune,
            };
            let (_, stats) = fetch(
                &agent,
                &server,
                &asset,
                &output,
                &cache,
                pubkey.as_deref(),
                !no_resume,
                &hybrid_opts,
                connections,
            )?;
            if let Some(path) = stats_json {
                std::fs::write(&path, stats.to_json())
                    .with_context(|| format!("cannot write {}", path.display()))?;
            }
            Ok(())
        }
        Command::FetchStatic {
            base,
            asset,
            output,
            cache,
            connections,
            ca,
            pubkey,
        } => {
            let agent = build_agent(ca.as_deref())?;
            let source = serverless::StaticSource::parse(&base, agent);
            let cache = ChunkCache::open(&cache)?;
            let pk_hex = match &pubkey {
                Some(pk) => Some(load_pubkey(pk)?),
                None => None,
            };
            serverless::fetch_static(
                &source,
                &asset,
                &output,
                &cache,
                connections,
                pk_hex.as_deref(),
            )?;
            Ok(())
        }
        Command::Resume {
            cache,
            asset,
            ca,
            pubkey,
        } => {
            let agent = build_agent(ca.as_deref())?;
            let pending: Vec<ResumeJournal> = ResumeJournal::list(&cache)
                .into_iter()
                .filter(|j| asset.as_deref().is_none_or(|a| a == j.asset))
                .collect();
            if pending.is_empty() {
                println!("nothing to resume");
                return Ok(());
            }
            let mut failures = 0u32;
            for j in pending {
                eprintln!(
                    "[resume] {} from {} -> {}",
                    j.asset,
                    j.server,
                    j.output.display()
                );
                if let Err(e) = fetch(
                    &agent,
                    &j.server,
                    &j.asset,
                    &j.output,
                    &cache,
                    pubkey.as_deref(),
                    true,
                    &HybridOpts {
                        enabled: true,
                        ..HybridOpts::default()
                    },
                    0,
                ) {
                    eprintln!("[resume] {} failed: {e:#}", j.asset);
                    failures += 1;
                }
            }
            if failures > 0 {
                bail!("{failures} resume(s) failed");
            }
            Ok(())
        }
        Command::Cache { action } => run_cache_action(action),
        Command::Play {
            server,
            asset,
            cache,
            ca,
            pubkey,
        } => {
            let agent = build_agent(ca.as_deref())?;
            let tmp = tempfile::tempdir()?;
            let (primaries, _) = fetch(
                &agent,
                &server,
                &asset,
                tmp.path(),
                &cache,
                pubkey.as_deref(),
                true,
                &HybridOpts {
                    enabled: true,
                    ..HybridOpts::default()
                },
                0,
            )?;
            let Some(target) = primaries.first() else {
                bail!("no playable track in asset {asset}");
            };
            eprintln!("[play] launching ffplay on {}", target.display());
            let status = std::process::Command::new("ffplay")
                .args(["-hide_banner", "-loglevel", "error", "-autoexit"])
                .arg(target)
                .status()
                .context("failed to spawn ffplay (is it installed?)")?;
            if !status.success() {
                bail!("ffplay exited with an error");
            }
            Ok(())
        }
    }
}

fn run_cache_action(action: CacheAction) -> Result<()> {
    match action {
        CacheAction::Verify { cache, delete } => {
            let cache = ChunkCache::open(&cache)?;
            let report = cache.verify(delete)?;
            println!(
                "cache   : {} chunks, {}",
                report.total,
                human_bytes(report.total_bytes)
            );
            if report.corrupt == 0 {
                println!("verify  : OK — every entry matches its hash");
            } else {
                println!(
                    "verify  : {} — {} corrupt entr{} {}",
                    ErrorCode::CacheCorruptRecoverable,
                    report.corrupt,
                    if report.corrupt == 1 { "y" } else { "ies" },
                    if delete {
                        "deleted"
                    } else {
                        "quarantined (they will be re-fetched on the next update)"
                    }
                );
            }
            Ok(())
        }
        CacheAction::Repair {
            server,
            asset,
            cache,
            ca,
        } => {
            let agent = build_agent(ca.as_deref())?;
            let cache = ChunkCache::open(&cache)?;
            let manifest_bytes =
                http_get_manifest(&agent, &format!("{server}/api/assets/{asset}/manifest"))?;
            let manifest = decode_manifest(&manifest_bytes)?.manifest;
            let mut present = 0u64;
            let mut repaired = 0u64;
            for hex in manifest_chunk_hashes(&manifest) {
                let Some(hash) = cavs_hash::from_hex(&hex) else {
                    bail!("bad chunk hash {hex} in manifest");
                };
                // get() verifies and drops corrupt entries, so one pass
                // covers both "missing" and "corrupt".
                if cache.get(&hash)?.is_some() {
                    present += 1;
                    continue;
                }
                let raw =
                    http_get_bytes(&agent, &format!("{server}/api/assets/{asset}/chunks/{hex}"))?;
                if cavs_hash::hash_chunk(&raw) != hash {
                    bail!(
                        "{}",
                        ErrorCode::ChunkHashMismatch
                            .msg(format!("repaired chunk {hex} failed hash verification"))
                    );
                }
                cache.put(&hash, &raw)?;
                repaired += 1;
            }
            println!("repair  : {present} chunks already valid, {repaired} re-fetched");
            Ok(())
        }
        CacheAction::Gc { cache, max_size } => {
            let budget = parse_size(&max_size)?;
            let cache = ChunkCache::open(&cache)?;
            let report = cache.gc(budget)?;
            println!(
                "gc      : {} of {} evicted ({} of {}) to fit {}",
                report.evicted,
                report.total_entries,
                human_bytes(report.evicted_bytes),
                human_bytes(report.total_bytes),
                human_bytes(budget)
            );
            Ok(())
        }
    }
}

/// Parse a human size: plain bytes, or a KiB/MiB/GiB/TiB (KB/MB/GB/TB)
/// suffix — all 1024-based.
fn parse_size(s: &str) -> Result<u64> {
    let t = s.trim();
    let split = t
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(t.len());
    let (num, suffix) = t.split_at(split);
    let value: f64 = num
        .parse()
        .map_err(|_| anyhow!("cannot parse size {s:?}"))?;
    let mult: u64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        other => bail!("unknown size suffix {other:?} in {s:?}"),
    };
    Ok((value * mult as f64) as u64)
}

/// Decode manifest bytes with the structured error codes attached.
fn decode_manifest(bytes: &[u8]) -> Result<cavs_manifest::LoadedManifest> {
    cavs_manifest::read_manifest(bytes).map_err(|e| match e {
        cavs_manifest::ManifestError::UnsupportedVersion(_) => {
            anyhow!(ErrorCode::UnsupportedManifestVersion.msg(e))
        }
        e => anyhow!(ErrorCode::ManifestCorrupt.msg(format!("bad manifest: {e}"))),
    })
}

/// Exact fetch statistics, exportable as JSON for benchmarking.
/// `inline_bytes` counts wire payload bytes (as transmitted, possibly
/// compressed); `inline_raw_bytes` counts the same payloads uncompressed.
pub struct FetchStats {
    pub inline_bytes: u64,
    pub inline_raw_bytes: u64,
    pub inline_chunks: u64,
    pub refs: u64,
    pub logical_bytes: u64,
    /// Route taken: "chunks", "references", "bootstrap" (v2 dual route),
    /// "no-op" or "previous-copy" (v0.6.0 no-op detection).
    pub delivery_mode: &'static str,
    /// Chunks inserted into the cache by slicing the bootstrap artifact.
    pub seeded_chunks: u64,
    /// Time spent seeding the cache from the bootstrap, in ms.
    pub seed_ms: u64,
    /// Manifest overhead metrics (v0.3.0 compact manifest).
    pub manifest: ManifestStats,
    /// The whole fetch was skipped: outputs already matched (v0.6.0).
    pub no_op: bool,
    /// Directory mode: files skipped because they already matched.
    pub no_op_files: u64,
    pub no_op_bytes: u64,
    /// Per-source byte accounting of the plan executor (v0.6.0 hybrid).
    pub sources: Option<hybrid::ExecOutcome>,
    /// Aggregated reconstruction-plan stats (v0.6.0 hybrid).
    pub plan: Option<cavs_rebuild_plan::PlanStats>,
}

/// How the manifest arrived and what it cost (v0.3.0 baseline metrics).
#[derive(Clone)]
pub struct ManifestStats {
    /// Wire format served: "json-v1" or "binary-v2".
    pub format: &'static str,
    /// Bytes of the manifest response body.
    pub wire_bytes: u64,
    /// Time to decode the manifest into the runtime model, in ms.
    pub parse_ms: f64,
    /// Chunk references across all tracks/segments (with repetition).
    pub chunk_count_logical: u64,
    /// Distinct chunk hashes.
    pub chunk_count_unique: u64,
}

impl FetchStats {
    /// Stats with every v0.6.0 extension zeroed (bootstrap / plain routes).
    #[allow(clippy::too_many_arguments)]
    fn v05(
        inline_bytes: u64,
        inline_raw_bytes: u64,
        inline_chunks: u64,
        refs: u64,
        logical_bytes: u64,
        delivery_mode: &'static str,
        seeded_chunks: u64,
        seed_ms: u64,
        manifest: ManifestStats,
    ) -> Self {
        FetchStats {
            inline_bytes,
            inline_raw_bytes,
            inline_chunks,
            refs,
            logical_bytes,
            delivery_mode,
            seeded_chunks,
            seed_ms,
            manifest,
            no_op: false,
            no_op_files: 0,
            no_op_bytes: 0,
            sources: None,
            plan: None,
        }
    }

    fn to_json(&self) -> String {
        let mut v = serde_json::json!({
            "inline_bytes": self.inline_bytes,
            "inline_raw_bytes": self.inline_raw_bytes,
            "inline_chunks": self.inline_chunks,
            "refs": self.refs,
            "logical_bytes": self.logical_bytes,
            "delivery_mode": self.delivery_mode,
            "seeded_chunks": self.seeded_chunks,
            "seed_ms": self.seed_ms,
            "no_op": self.no_op,
            "no_op_files": self.no_op_files,
            "no_op_bytes": self.no_op_bytes,
            "manifest": {
                "format": self.manifest.format,
                "wire_bytes": self.manifest.wire_bytes,
                "parse_ms": self.manifest.parse_ms,
                "chunk_count_logical": self.manifest.chunk_count_logical,
                "chunk_count_unique": self.manifest.chunk_count_unique,
            },
        });
        if let Some(s) = &self.sources {
            v["sources"] = serde_json::json!({
                "network_bytes": self.inline_bytes + s.repair_wire_bytes,
                "cache_chunk_bytes": s.cache_chunk_bytes,
                "previous_artifact_bytes": s.previous_artifact_bytes,
                "repair_wire_bytes": s.repair_wire_bytes,
                "demoted_chunks": s.demoted_chunks,
            });
        }
        if let Some(p) = &self.plan {
            v["reconstruction_plan"] = serde_json::to_value(p).unwrap_or_default();
        }
        v.to_string()
    }
}

/// HTTP agent; with `--ca`, trusts exactly that PEM certificate (dev TLS).
fn build_agent(ca: Option<&Path>) -> Result<ureq::Agent> {
    let Some(ca_path) = ca else {
        return Ok(ureq::AgentBuilder::new().build());
    };
    let pem = std::fs::File::open(ca_path)
        .with_context(|| format!("cannot open CA file {}", ca_path.display()))?;
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut std::io::BufReader::new(pem)) {
        roots
            .add(cert.context("reading certificate")?)
            .context("adding certificate to trust store")?;
    }
    if roots.is_empty() {
        anyhow::bail!("{} contains no certificates", ca_path.display());
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(ureq::AgentBuilder::new()
        .tls_config(std::sync::Arc::new(config))
        .build())
}

/// Verify the manifest's Ed25519 content signature against a trusted key.
/// Checks: signer matches, Merkle root recomputes from the chunk table, the
/// signature verifies, and every referenced chunk is covered by the table.
fn verify_manifest_signature(manifest: &Manifest, trusted_pubkey_hex: &str) -> Result<()> {
    let sig_hex = manifest
        .signature
        .as_deref()
        .context("asset is not signed but --pubkey was given")?;
    let signer_hex = manifest
        .signer_pubkey
        .as_deref()
        .context("asset signature has no public key")?;
    if !signer_hex.eq_ignore_ascii_case(trusted_pubkey_hex) {
        anyhow::bail!(
            "asset is signed by an untrusted key: {signer_hex} (expected {trusted_pubkey_hex})"
        );
    }

    let leaves: Vec<cavs_hash::ChunkHash> = manifest
        .chunk_table
        .iter()
        .map(|h| cavs_hash::from_hex(h).context("bad hash in chunk_table"))
        .collect::<Result<_>>()?;
    let root = cavs_hash::merkle_root(&leaves);
    if !manifest.merkle_root.eq_ignore_ascii_case(&to_hex(&root)) {
        anyhow::bail!("manifest merkle_root does not match its chunk_table");
    }

    let pk_bytes: [u8; 32] = decode_hex(signer_hex, 32)?.try_into().unwrap();
    let sig_bytes: [u8; 64] = decode_hex(sig_hex, 64)?.try_into().unwrap();
    let key =
        ed25519_dalek::VerifyingKey::from_bytes(&pk_bytes).context("invalid signer public key")?;
    let message = cavs_hash::content_signature_message(&root, leaves.len() as u64);
    use ed25519_dalek::Verifier;
    key.verify(&message, &ed25519_dalek::Signature::from_bytes(&sig_bytes))
        .map_err(|_| anyhow::anyhow!("content signature is INVALID"))?;

    // Every chunk the manifest references must be covered by the signed table.
    let table: std::collections::HashSet<&str> =
        manifest.chunk_table.iter().map(|s| s.as_str()).collect();
    for hash in manifest_chunk_hashes(manifest) {
        if !table.contains(hash.as_str()) {
            anyhow::bail!("chunk {hash} referenced but not covered by the signed table");
        }
    }
    Ok(())
}

/// Accept a literal 64-hex Ed25519 key or a path to a `.pub` file, and
/// return the hex string.
fn load_pubkey(pk: &str) -> Result<String> {
    if pk.len() == 64 && pk.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(pk.to_string())
    } else {
        Ok(std::fs::read_to_string(pk)
            .with_context(|| format!("cannot read pubkey file {pk}"))?
            .trim()
            .to_string())
    }
}

fn decode_hex(s: &str, len: usize) -> Result<Vec<u8>> {
    if s.len() != len * 2 {
        anyhow::bail!("expected {} hex chars, got {}", len * 2, s.len());
    }
    (0..len)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).context("bad hex"))
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn fetch(
    agent: &ureq::Agent,
    server: &str,
    asset: &str,
    output: &Path,
    cache_dir: &Path,
    pubkey: Option<&str>,
    resume: bool,
    hybrid_opts: &HybridOpts,
    connections: usize,
) -> Result<(Vec<PathBuf>, FetchStats)> {
    let cache = ChunkCache::open(cache_dir)?;

    // 1. Manifest (+ optional signature enforcement). We ask for the compact
    //    binary v2 format; v1 servers ignore the Accept header and reply
    //    JSON, which read_manifest detects from the bytes themselves.
    let manifest_bytes =
        http_get_manifest(agent, &format!("{server}/api/assets/{asset}/manifest"))?;
    let parse_started = std::time::Instant::now();
    let loaded = decode_manifest(&manifest_bytes)?;
    let manifest = loaded.manifest;
    let manifest_b3 = to_hex(&cavs_hash::hash_chunk(&manifest_bytes));

    // Resume journal (v0.5.0): honour a prior interrupted fetch only when
    // it was against these exact manifest bytes; anything stale is
    // discarded together with its partial artifacts.
    let prior = match ResumeJournal::load(cache_dir, asset) {
        Some(j) if !resume => {
            j.discard(cache_dir);
            None
        }
        Some(j) if j.server != server || j.manifest_blake3 != manifest_b3 => {
            eprintln!("[resume] journal for {asset} is stale (asset republished or different server); starting clean");
            j.discard(cache_dir);
            None
        }
        Some(j) => {
            eprintln!("[resume] continuing interrupted fetch of {asset}");
            Some(j)
        }
        None => None,
    };
    let manifest_stats = ManifestStats {
        format: loaded.format.label(),
        wire_bytes: manifest_bytes.len() as u64,
        parse_ms: parse_started.elapsed().as_secs_f64() * 1000.0,
        chunk_count_logical: manifest
            .tracks
            .iter()
            .map(|t| t.init_chunks.len() as u64)
            .chain(manifest.segments.iter().map(|s| s.chunks.len() as u64))
            .sum(),
        chunk_count_unique: if manifest.chunk_table.is_empty() {
            manifest_chunk_hashes(&manifest).len() as u64
        } else {
            manifest.chunk_table.len() as u64
        },
    };
    eprintln!(
        "[fetch] manifest {}: {} wire, parsed in {:.2} ms",
        manifest_stats.format,
        human_bytes(manifest_stats.wire_bytes),
        manifest_stats.parse_ms
    );
    if let Some(pk) = pubkey {
        // Accept a literal hex key or a path to a .pub file.
        let pk_hex = if pk.len() == 64 && pk.chars().all(|c| c.is_ascii_hexdigit()) {
            pk.to_string()
        } else {
            std::fs::read_to_string(pk)
                .with_context(|| format!("cannot read pubkey file {pk}"))?
                .trim()
                .to_string()
        };
        verify_manifest_signature(&manifest, &pk_hex)
            .map_err(|e| anyhow!(ErrorCode::SignatureInvalid.msg(format!("{e:#}"))))?;
        eprintln!("[fetch] content signature OK (signer {})", &pk_hex[..16]);
    }

    // v0.6.0 hybrid setup: container payloads (raw single/multi-file packs
    // and directory trees) can reuse verified bytes from disk.
    let payload_kind = manifest
        .meta
        .iter()
        .find(|(k, _)| k == "payload")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let is_container = payload_kind == "raw" || payload_kind == "directory";
    let sha_by_name: std::collections::HashMap<String, String> = manifest
        .meta
        .iter()
        .filter_map(|(k, v)| {
            k.strip_prefix("sha256:")
                .map(|n| (n.to_string(), v.clone()))
        })
        .collect();
    let data_track_names: Vec<String> = manifest
        .tracks
        .iter()
        .filter(|t| t.kind != "video" && t.kind != "audio")
        .map(|t| t.name.clone())
        .collect();

    // No-op level 1: every output file already exists and matches its
    // manifest digest — nothing to download, nothing to rewrite.
    if is_container
        && !hybrid_opts.force_reconstruct
        && !data_track_names.is_empty()
        && data_track_names.len() == manifest.tracks.len()
    {
        let mut matched_bytes = 0u64;
        let all_match = manifest.tracks.iter().all(|t| {
            let Some(expected) = sha_by_name.get(&t.name) else {
                return false;
            };
            if t.name.contains("..") || t.name.starts_with('/') {
                return false;
            }
            let target = output.join(&t.name);
            if hybrid::file_matches_sha256(&target, expected) {
                matched_bytes += std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
                true
            } else {
                false
            }
        });
        if all_match {
            if let Some(j) = ResumeJournal::load(cache_dir, asset) {
                j.discard(cache_dir);
            }
            eprintln!(
                "[fetch] no-op: {} output file(s) already match the target hashes",
                manifest.tracks.len()
            );
            let primaries = manifest
                .tracks
                .iter()
                .filter(|t| t.codec == "raw")
                .map(|t| output.join(&t.name))
                .collect();
            let mut stats = FetchStats::v05(
                0,
                0,
                0,
                0,
                manifest_logical_bytes(&manifest),
                "no-op",
                0,
                0,
                manifest_stats,
            );
            stats.no_op = true;
            stats.no_op_files = manifest.tracks.len() as u64;
            stats.no_op_bytes = matched_bytes;
            return Ok((primaries, stats));
        }
    }

    // Previous installed artifact: open, chunk with the packer's profile
    // and index by the hashes this manifest needs. Unusable previous
    // artifacts degrade to a warning, never a failure.
    let mut prev: Option<hybrid::PreviousArtifact> = None;
    if hybrid_opts.enabled && is_container {
        if let Some(path) = &hybrid_opts.previous_artifact {
            if !path.is_file() {
                eprintln!(
                    "[hybrid] {}",
                    ErrorCode::PreviousArtifactMissing.msg(format!(
                        "{} not found; continuing without it",
                        path.display()
                    ))
                );
            } else {
                // No-op level 2: the previous artifact IS the new version
                // (single-file assets): install it locally, zero network.
                if !hybrid_opts.force_reconstruct
                    && data_track_names.len() == 1
                    && manifest.tracks.len() == 1
                {
                    let name = &data_track_names[0];
                    if let Some(expected) = sha_by_name.get(name) {
                        if !name.contains("..")
                            && !name.starts_with('/')
                            && hybrid::file_matches_sha256(path, expected)
                        {
                            eprintln!(
                                "[fetch] no-op: previous artifact already matches the target; copying locally"
                            );
                            let final_path = output.join(name);
                            std::fs::create_dir_all(output)?;
                            let mut part = PartFile::create(final_path.clone(), true)?;
                            let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
                            let mut buf = [0u8; 64 * 1024];
                            let mut copied = 0u64;
                            loop {
                                use std::io::Read as _;
                                let n = reader.read(&mut buf)?;
                                if n == 0 {
                                    break;
                                }
                                part.append_bytes(&buf[..n])?;
                                copied += n as u64;
                            }
                            let installed = part.finish(Some(expected))?;
                            if let Some(j) = ResumeJournal::load(cache_dir, asset) {
                                j.discard(cache_dir);
                            }
                            let mut stats = FetchStats::v05(
                                0,
                                0,
                                0,
                                0,
                                manifest_logical_bytes(&manifest),
                                "previous-copy",
                                0,
                                0,
                                manifest_stats,
                            );
                            stats.no_op = true;
                            stats.no_op_files = 1;
                            stats.no_op_bytes = copied;
                            return Ok((vec![installed], stats));
                        }
                    }
                }
                let needed: std::collections::HashSet<String> =
                    manifest_chunk_hashes(&manifest).into_iter().collect();
                let profile_label = data_track_names.first().and_then(|name| {
                    let key = format!("profile:{name}");
                    manifest
                        .meta
                        .iter()
                        .find(|(k, _)| *k == key)
                        .map(|(_, v)| v.as_str())
                });
                let mode = hybrid::mode_from_profile_label(profile_label);
                match hybrid::PreviousArtifact::open_and_index(path, mode, &needed) {
                    Ok(p) => {
                        eprintln!(
                            "[hybrid] previous artifact {}: {} of {} chunks reusable (indexed in {} ms)",
                            path.display(),
                            p.index.len(),
                            needed.len(),
                            p.indexed_ms
                        );
                        prev = Some(p);
                    }
                    Err(e) => eprintln!(
                        "[hybrid] previous artifact unusable ({e:#}); continuing without it"
                    ),
                }
            }
        }
    }

    // 2. Announce our have-set (intersecting locally with the manifest keeps
    //    the request small: only hashes this asset actually uses). Large
    //    have-sets are summarised with a Bloom filter so the session-open
    //    body stays compact; false positives are repaired in step 3b.
    // Previous-artifact matches count as "have": the server sends refs for
    // them and the plan executor reads them from the old file directly.
    let have: Vec<String> = manifest_chunk_hashes(&manifest)
        .into_iter()
        .filter(|h| cache.contains(h) || prev.as_ref().is_some_and(|p| p.index.contains_key(h)))
        .collect();
    let open_req = if have.len() > BLOOM_THRESHOLD {
        let mut bloom = cavs_proto::BloomFilter::with_capacity(have.len());
        for hex in &have {
            if let Some(h) = cavs_hash::from_hex(hex) {
                bloom.insert(&h);
            }
        }
        serde_json::to_string(&SessionOpenRequest {
            have: Vec::new(),
            have_bloom: Some(bloom),
        })?
    } else {
        serde_json::to_string(&SessionOpenRequest {
            have: have.clone(),
            have_bloom: None,
        })?
    };
    let session: SessionOpenResponse = serde_json::from_str(&http_post_json(
        agent,
        &format!("{server}/api/assets/{asset}/sessions"),
        &open_req,
    )?)?;
    eprintln!(
        "[fetch] session {} (server matched {} cached chunks)",
        session.session_id, session.known_chunks
    );

    // v0.6.0: the server's bootstrap suggestion assumes the have-set is the
    // whole local state. When a previous artifact covers most of the asset,
    // the chunk path (missing chunks only) beats re-downloading the full
    // artifact — override the advisory route in that case. The raw missing
    // estimate overstates the compressed wire cost, so this never picks the
    // chunk path when the bootstrap is actually cheaper.
    let mut take_bootstrap =
        session.delivery_mode.as_deref() == Some(cavs_proto::DELIVERY_BOOTSTRAP);
    if take_bootstrap && prev.as_ref().is_some_and(|p| !p.index.is_empty()) {
        let have_set: std::collections::HashSet<&str> = have.iter().map(|s| s.as_str()).collect();
        let mut seen = std::collections::HashSet::new();
        let mut missing_raw = 0u64;
        for t in &manifest.tracks {
            for c in &t.init_chunks {
                if !have_set.contains(c.hash.as_str()) && seen.insert(c.hash.as_str()) {
                    missing_raw += c.len as u64;
                }
            }
        }
        for s in &manifest.segments {
            for c in &s.chunks {
                if !have_set.contains(c.hash.as_str()) && seen.insert(c.hash.as_str()) {
                    missing_raw += c.len as u64;
                }
            }
        }
        let bootstrap_size = session.bootstrap_size.unwrap_or(u64::MAX);
        if bootstrap_size >= missing_raw {
            eprintln!(
                "[hybrid] previous artifact makes the chunk path cheaper ({} missing vs {} bootstrap); overriding route",
                human_bytes(missing_raw),
                human_bytes(bootstrap_size)
            );
            take_bootstrap = false;
        }
    }

    // v2 dual route: for a cold cache the server may have measured that the
    // full compressed artifact is cheaper than the chunk path. Download it,
    // verify, install, and seed the local chunk cache from it — so the NEXT
    // fetch (an update) pays only for what changed. Any failure falls back
    // to the normal chunk path below.
    if take_bootstrap {
        match fetch_bootstrap(
            agent,
            server,
            asset,
            &manifest,
            &session,
            &cache,
            output,
            &manifest_stats,
            cache_dir,
            &manifest_b3,
            prior.as_ref(),
        ) {
            Ok(result) => return Ok(result),
            Err(e) => {
                // The journal (and any .zst.part) stays on disk: a later
                // fetch/resume continues the bootstrap download where it
                // stopped, while this run falls back to the chunk path.
                eprintln!("[fetch] bootstrap route failed ({e:#}); falling back to chunks")
            }
        }
    }

    // Chunk route: progress lives in the chunk cache itself, so the journal
    // only needs to say "a fetch of this asset is in flight". A journal
    // left by an interrupted bootstrap download is kept as-is — its
    // partial artifact is worth more than this marker.
    let bootstrap_in_flight = ResumeJournal::load(cache_dir, asset)
        .is_some_and(|j| j.state == ResumeState::BootstrapDownloading);
    if !bootstrap_in_flight {
        let _ = ResumeJournal {
            asset: asset.to_string(),
            server: server.to_string(),
            output: output.to_path_buf(),
            manifest_blake3: manifest_b3.clone(),
            state: ResumeState::ChunkDownloading,
            bootstrap_part: None,
            bootstrap_blake3: None,
            updated_at: journal::now_unix(),
        }
        .save(cache_dir);
    }

    // 3. Fetch the chunks this client lacks and land them verified in the
    //    disk cache — nothing accumulates in RAM (the content-addressable
    //    cache IS the store). References are only counted; reconstruction
    //    reads them from the cache.
    let mut inline_bytes = 0u64;
    let mut inline_raw_bytes = 0u64;
    let mut inline_count = 0u64;
    let mut ref_count = 0u64;

    // v1.4.0: for container payloads (builds / directory trees) the
    // client computes its own missing set from the manifest and downloads
    // those immutable chunks by hash, concurrently, from the edge-cacheable
    // chunk endpoint. This replaces the sequential session/batch round-trips
    // for the generic file path; `--connections 0` (or a media payload) keeps
    // the legacy session/batch stream.
    let use_parallel = connections >= 1 && is_container;
    if use_parallel {
        // Unique chunks the asset needs, in manifest order (read locality),
        // minus whatever the cache or the previous artifact already provides.
        let mut seen = std::collections::HashSet::new();
        let mut missing: Vec<cavs_hash::ChunkHash> = Vec::new();
        let mut have_local = 0u64;
        for hex in manifest_chunk_hashes(&manifest) {
            if !seen.insert(hex.clone()) {
                continue;
            }
            let present =
                cache.contains(&hex) || prev.as_ref().is_some_and(|p| p.index.contains_key(&hex));
            if present {
                have_local += 1;
            } else if let Some(h) = cavs_hash::from_hex(&hex) {
                missing.push(h);
            }
        }
        ref_count = have_local;
        eprintln!(
            "[fetch] parallel chunk fetch: {} missing, {} already local, {} connection(s)",
            missing.len(),
            have_local,
            connections
        );
        let pstats =
            parallel::fetch_chunks_parallel(agent, server, asset, &missing, &cache, connections)?;
        inline_bytes = pstats.wire_bytes;
        inline_raw_bytes = pstats.raw_bytes;
        inline_count = pstats.count;
    } else {
        // Legacy session/batch stream. Refs the server assumed we had (bloom
        // false positives) but our cache actually lacks are repaired after
        // the loop by a direct by-hash fetch.
        let mut missing_refs: Vec<cavs_hash::ChunkHash> = Vec::new();
        let all_tracks: Vec<u32> = manifest.tracks.iter().map(|t| t.track_id).collect();
        let mut segment_ids: Vec<u64> = manifest.segments.iter().map(|s| s.segment_id).collect();
        segment_ids.sort_unstable();

        let mut first = true;
        for group in segment_ids.chunks(BATCH_SIZE.max(1)) {
            let req = BatchRequest {
                track_inits: if first {
                    all_tracks.clone()
                } else {
                    Vec::new()
                },
                segment_ids: group.to_vec(),
            };
            first = false;
            let mut reader = http_post_reader(
                agent,
                &format!("{server}/api/sessions/{}/batch", session.session_id),
                &serde_json::to_string(&req)?,
            )?;
            cavs_proto::decode_stream(&mut reader, |item| {
                let cavs_proto::BatchItem::Instr(instr) = item else {
                    return Ok(());
                };
                let hex = to_hex(instr.hash());
                match instr {
                    DeliveryInstr::Inline {
                        hash,
                        len_raw,
                        compression,
                        payload,
                    } => {
                        inline_bytes += payload.len() as u64;
                        let raw = match compression {
                            cavs_proto::WIRE_COMPRESSION_NONE => payload,
                            cavs_proto::WIRE_COMPRESSION_ZSTD => {
                                zstd::bulk::decompress(&payload, len_raw as usize)
                                    .map_err(|e| format!("descomprimiendo chunk {hex}: {e}"))?
                            }
                            other => return Err(format!("unknown wire compression {other}")),
                        };
                        if raw.len() != len_raw as usize || cavs_hash::hash_chunk(&raw) != hash {
                            return Err(ErrorCode::ChunkHashMismatch
                                .msg(format!("inline chunk {hex} failed hash verification")));
                        }
                        cache.put(&hash, &raw).map_err(|e| e.to_string())?;
                        inline_raw_bytes += raw.len() as u64;
                        inline_count += 1;
                    }
                    DeliveryInstr::Ref { hash } => {
                        ref_count += 1;
                        // Bloom false positive: server thinks we have it, but
                        // neither the cache nor the previous artifact does.
                        if !cache.contains(&hex)
                            && !prev.as_ref().is_some_and(|p| p.index.contains_key(&hex))
                        {
                            missing_refs.push(hash);
                        }
                    }
                }
                Ok(())
            })
            .map_err(|e| anyhow::anyhow!("bad batch payload: {e}"))?;
        }

        // 3b. Repair bloom false positives: fetch each missing referenced
        //     chunk directly by hash, verify and cache it.
        missing_refs.sort_unstable();
        missing_refs.dedup();
        if !missing_refs.is_empty() {
            eprintln!(
                "[fetch] repairing {} bloom false-positive ref(s)",
                missing_refs.len()
            );
            for hash in &missing_refs {
                let hex = to_hex(hash);
                let raw =
                    http_get_bytes(agent, &format!("{server}/api/assets/{asset}/chunks/{hex}"))?;
                if cavs_hash::hash_chunk(&raw) != *hash {
                    bail!(
                        "{}",
                        ErrorCode::ChunkHashMismatch
                            .msg(format!("repaired chunk {hex} failed hash verification"))
                    );
                }
                cache.put(hash, &raw)?;
                inline_bytes += raw.len() as u64;
                inline_raw_bytes += raw.len() as u64;
                inline_count += 1;
            }
        }
    }

    // 4. Reconstruction. Container payloads (raw/directory) go through the
    //    unified plan executor (v0.6.0): every data track gets a
    //    ReconstructionPlan whose sources are the previous artifact, the
    //    chunk cache and the just-fetched network chunks. Media payloads
    //    keep the v0.5 streaming path.
    let (primaries, recon) = if is_container {
        reconstruct_with_plans(
            agent,
            server,
            asset,
            &manifest,
            &payload_kind,
            &cache,
            output,
            prev.as_ref(),
            hybrid_opts,
        )?
    } else {
        (
            reconstruct_streaming(&manifest, &cache, output)?,
            ReconOutcome::default(),
        )
    };
    if let Some(path) = &hybrid_opts.dump_plan {
        std::fs::write(path, serde_json::to_string_pretty(&recon.plans)?)
            .with_context(|| format!("cannot write {}", path.display()))?;
        eprintln!("[hybrid] plan dumped to {}", path.display());
    }

    // The fetch is complete and verified: drop the journal and any
    // leftover bootstrap partial from an earlier attempt.
    if let Some(j) = ResumeJournal::load(cache_dir, asset) {
        j.discard(cache_dir);
    }

    let logical: u64 = manifest_logical_bytes(&manifest);
    println!(
        "fetched : {asset} -> {} ({} tracks, {} segments)",
        output.display(),
        manifest.tracks.len(),
        manifest.segments.len()
    );
    println!(
        "egress  : {} wire inline ({} raw, {} chunks) / {} refs resolved from cache",
        human_bytes(inline_bytes),
        human_bytes(inline_raw_bytes),
        inline_count,
        ref_count
    );
    println!(
        "logical : {}  -> saved {:.2}% of egress",
        human_bytes(logical),
        if logical == 0 {
            0.0
        } else {
            (logical.saturating_sub(inline_bytes)) as f64 * 100.0 / logical as f64
        }
    );
    if let Some(outcome) = &recon.sources {
        println!(
            "sources : {} previous artifact / {} cache / {} network repair",
            human_bytes(outcome.previous_artifact_bytes),
            human_bytes(outcome.cache_chunk_bytes),
            human_bytes(outcome.repair_wire_bytes),
        );
    }
    let mut stats = FetchStats::v05(
        inline_bytes,
        inline_raw_bytes,
        inline_count,
        ref_count,
        logical,
        if inline_count == 0 {
            "references"
        } else {
            "chunks"
        },
        0,
        0,
        manifest_stats,
    );
    stats.no_op_files = recon.no_op_files;
    stats.no_op_bytes = recon.no_op_bytes;
    stats.sources = recon.sources;
    stats.plan = recon.plan;
    Ok((primaries, stats))
}

/// The v2 bootstrap route: download the whole compressed artifact, verify it
/// end to end, install it atomically, and seed the local chunk cache by
/// slicing the installed file along the manifest's chunk plan. Constant
/// memory: the artifact streams to disk and chunks are read back one at a
/// time. An interrupted download leaves the `.zst.part` plus a journal
/// entry; the next attempt continues it with an HTTP Range request
/// (v0.5.0) — the artifact is immutable, so the resumed bytes are the
/// same bytes, and the final BLAKE3 check still covers the whole file.
#[allow(clippy::too_many_arguments)]
fn fetch_bootstrap(
    agent: &ureq::Agent,
    server: &str,
    asset: &str,
    manifest: &Manifest,
    session: &SessionOpenResponse,
    cache: &ChunkCache,
    output: &Path,
    manifest_stats: &ManifestStats,
    cache_dir: &Path,
    manifest_b3: &str,
    prior: Option<&ResumeJournal>,
) -> Result<(Vec<PathBuf>, FetchStats)> {
    // The bootstrap covers exactly one raw data track (the packer only emits
    // it for single-input packs). Anything else falls back to chunks.
    let boot_name = manifest
        .meta
        .iter()
        .find(|(k, _)| k == "bootstrap.name")
        .map(|(_, v)| v.as_str())
        .context("manifest has no bootstrap.name meta")?;
    if manifest.tracks.len() != 1 {
        bail!("bootstrap requires a single-track asset");
    }
    let track = &manifest.tracks[0];
    if track.name != boot_name {
        bail!("bootstrap.name does not match the asset's track");
    }
    if track.name.contains("..") || track.name.starts_with('/') {
        bail!("unsafe track name: {}", track.name);
    }

    // 1. Stream the artifact to disk, hashing the wire bytes as they arrive.
    //    A valid prior journal + partial file continues from its length.
    std::fs::create_dir_all(output)?;
    let zst_path = output.join(format!("{boot_name}.bootstrap.zst.part"));
    let expected_b3 = session.bootstrap_blake3.as_deref();
    let mut resume_from = 0u64;
    if let (Some(p), Some(expected)) = (prior, expected_b3) {
        if p.state == ResumeState::BootstrapDownloading
            && p.bootstrap_part.as_deref() == Some(zst_path.as_path())
            && p.bootstrap_blake3.as_deref() == Some(expected)
        {
            resume_from = std::fs::metadata(&zst_path).map(|m| m.len()).unwrap_or(0);
        }
    }

    // Journal the download before it starts, so an interruption at any
    // point leaves enough to resume.
    let _ = ResumeJournal {
        asset: asset.to_string(),
        server: server.to_string(),
        output: output.to_path_buf(),
        manifest_blake3: manifest_b3.to_string(),
        state: ResumeState::BootstrapDownloading,
        bootstrap_part: Some(zst_path.clone()),
        bootstrap_blake3: expected_b3.map(str::to_string),
        updated_at: journal::now_unix(),
    }
    .save(cache_dir);

    let url = format!("{server}/api/assets/{asset}/bootstrap");
    let mut hasher = cavs_hash::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    let resp = if resume_from > 0 {
        // Hash the bytes we already have; the request continues after them.
        {
            use std::io::Read as _;
            let mut existing = std::io::BufReader::new(std::fs::File::open(&zst_path)?);
            loop {
                let n = existing.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
        }
        let resp = retry::with_retry(&format!("GET {url}"), || {
            agent
                .get(&url)
                .set("range", &format!("bytes={resume_from}-"))
                .call()
        })?;
        if resp.status() == 206 {
            eprintln!(
                "[resume] continuing bootstrap download at {}",
                human_bytes(resume_from)
            );
        } else {
            // Server ignored the range (older cavs-server): start over.
            resume_from = 0;
            hasher = cavs_hash::Hasher::new();
        }
        resp
    } else {
        retry::with_retry(&format!("GET {url}"), || agent.get(&url).call())?
    };

    let mut reader = resp.into_reader();
    let file = if resume_from > 0 {
        std::fs::File::options().append(true).open(&zst_path)?
    } else {
        std::fs::File::create(&zst_path)?
    };
    let mut file = std::io::BufWriter::new(file);
    let mut wire_bytes = 0u64;
    loop {
        use std::io::{Read as _, Write as _};
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
        wire_bytes += n as u64;
    }
    {
        use std::io::Write as _;
        file.flush()?;
    }
    drop(file);

    // 2. Verify the wire artifact against the server-announced BLAKE3.
    if let Some(expected) = expected_b3 {
        let got = to_hex(&hasher.finalize());
        if !got.eq_ignore_ascii_case(expected) {
            // Not resumable: these bytes are wrong, not incomplete.
            let _ = std::fs::remove_file(&zst_path);
            ResumeJournal::clear(cache_dir, asset);
            bail!(
                "{}",
                ErrorCode::BootstrapHashMismatch
                    .msg("bootstrap artifact failed BLAKE3 verification")
            );
        }
    }
    eprintln!(
        "[fetch] bootstrap artifact: {} wire{} (chunk path estimate: {})",
        human_bytes(wire_bytes),
        if resume_from > 0 {
            format!(" (+{} resumed)", human_bytes(resume_from))
        } else {
            String::new()
        },
        session
            .estimated_chunk_payload
            .map(human_bytes)
            .unwrap_or_else(|| "?".into()),
    );

    // 3. Decompress streaming into the final artifact, verifying the
    //    packer's SHA-256 end to end; atomic rename via PartFile.
    let expected_sha = manifest
        .meta
        .iter()
        .find(|(k, _)| k.strip_prefix("sha256:") == Some(boot_name))
        .map(|(_, v)| v.as_str());
    let final_path = output.join(&track.name);
    let mut part = PartFile::create(final_path.clone(), expected_sha.is_some())?;
    let mut raw_bytes = 0u64;
    {
        use std::io::Read as _;
        let zst_file = std::fs::File::open(&zst_path)?;
        let mut dec = zstd::stream::read::Decoder::new(std::io::BufReader::new(zst_file))?;
        loop {
            let n = dec.read(&mut buf)?;
            if n == 0 {
                break;
            }
            part.append_bytes(&buf[..n])?;
            raw_bytes += n as u64;
        }
    }
    let installed = part.finish(expected_sha)?;
    let _ = std::fs::remove_file(&zst_path);
    ResumeJournal::clear(cache_dir, asset);

    // 4. Seed the chunk cache from the installed artifact using the
    //    manifest's chunk plan: every future update starts warm.
    let seed_started = std::time::Instant::now();
    let mut seeded = 0u64;
    {
        use std::io::Read as _;
        let mut segs: Vec<_> = manifest
            .segments
            .iter()
            .filter(|s| s.track_id == track.track_id)
            .collect();
        segs.sort_by_key(|s| (s.pts_start, s.segment_id));
        let mut file = std::io::BufReader::new(std::fs::File::open(&installed)?);
        let mut chunk_buf = Vec::new();
        for seg in segs {
            for c in &seg.chunks {
                chunk_buf.resize(c.len as usize, 0);
                file.read_exact(&mut chunk_buf)
                    .with_context(|| format!("bootstrap shorter than chunk plan at {}", c.hash))?;
                let hash = cavs_hash::from_hex(&c.hash)
                    .with_context(|| format!("bad chunk hash {}", c.hash))?;
                if cavs_hash::hash_chunk(&chunk_buf) != hash {
                    bail!(
                        "{}",
                        ErrorCode::ChunkHashMismatch
                            .msg(format!("seeded chunk {} failed hash verification", c.hash))
                    );
                }
                cache.put(&hash, &chunk_buf)?;
                seeded += 1;
            }
        }
    }
    let seed_ms = seed_started.elapsed().as_millis() as u64;

    let logical = manifest_logical_bytes(manifest);
    println!(
        "fetched : {asset} -> {} (bootstrap route)",
        output.display()
    );
    println!(
        "egress  : {} wire bootstrap ({} raw) / cache seeded with {seeded} chunks in {seed_ms} ms",
        human_bytes(wire_bytes),
        human_bytes(raw_bytes),
    );
    println!(
        "logical : {}  -> saved {:.2}% of egress",
        human_bytes(logical),
        if logical == 0 {
            0.0
        } else {
            (logical.saturating_sub(wire_bytes)) as f64 * 100.0 / logical as f64
        }
    );
    Ok((
        vec![installed],
        FetchStats::v05(
            wire_bytes,
            raw_bytes,
            0,
            0,
            logical,
            "bootstrap",
            seeded,
            seed_ms,
            manifest_stats.clone(),
        ),
    ))
}

/// All chunk hashes an asset can reference, deduplicated.
fn manifest_chunk_hashes(manifest: &Manifest) -> Vec<String> {
    let mut set = std::collections::HashSet::new();
    for t in &manifest.tracks {
        for c in &t.init_chunks {
            set.insert(c.hash.clone());
        }
    }
    for s in &manifest.segments {
        for c in &s.chunks {
            set.insert(c.hash.clone());
        }
    }
    set.into_iter().collect()
}

fn manifest_logical_bytes(manifest: &Manifest) -> u64 {
    let mut total = 0u64;
    for t in &manifest.tracks {
        total += t.init_chunks.iter().map(|c| c.len as u64).sum::<u64>();
    }
    for s in &manifest.segments {
        total += s.chunks.iter().map(|c| c.len as u64).sum::<u64>();
    }
    total
}

/// Streaming writer for one output artifact: temp `.part` file, chunks
/// appended in order straight from the disk cache (one chunk in RAM at a
/// time, BLAKE3-verified by the cache read), optional SHA-256 running
/// digest, then atomic rename into place.
struct PartFile {
    file: std::io::BufWriter<std::fs::File>,
    part_path: PathBuf,
    final_path: PathBuf,
    hasher: Option<sha2::Sha256>,
}

impl PartFile {
    fn create(final_path: PathBuf, with_sha256: bool) -> Result<Self> {
        if let Some(parent) = final_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let part_path = final_path.with_extension(match final_path.extension() {
            Some(ext) => format!("{}.part", ext.to_string_lossy()),
            None => "part".to_string(),
        });
        let file = std::io::BufWriter::new(std::fs::File::create(&part_path)?);
        Ok(Self {
            file,
            part_path,
            final_path,
            hasher: with_sha256.then(|| {
                use sha2::Digest as _;
                sha2::Sha256::new()
            }),
        })
    }

    /// Append raw bytes (bootstrap decompression path), feeding the running
    /// SHA-256 when enabled.
    fn append_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        use std::io::Write as _;
        if let Some(h) = &mut self.hasher {
            use sha2::Digest as _;
            h.update(bytes);
        }
        self.file.write_all(bytes)?;
        Ok(())
    }

    fn append_chunk(&mut self, cache: &ChunkCache, hash_hex: &str) -> Result<()> {
        use std::io::Write as _;
        let hash =
            cavs_hash::from_hex(hash_hex).with_context(|| format!("bad chunk hash {hash_hex}"))?;
        let bytes = cache
            .get(&hash)?
            .with_context(|| format!("chunk {hash_hex} missing after fetch"))?;
        if let Some(h) = &mut self.hasher {
            use sha2::Digest as _;
            h.update(&bytes);
        }
        self.file.write_all(&bytes)?;
        Ok(())
    }

    /// Flush, optionally verify the SHA-256 against `expected_hex`, and
    /// rename `.part` -> final. On mismatch the `.part` is removed and the
    /// final path is never touched.
    fn finish(mut self, expected_sha256: Option<&str>) -> Result<PathBuf> {
        use std::io::Write as _;
        self.file.flush()?;
        drop(self.file);
        if let (Some(h), Some(expected)) = (self.hasher.take(), expected_sha256) {
            use sha2::Digest as _;
            let digest: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
            if !digest.eq_ignore_ascii_case(expected) {
                let _ = std::fs::remove_file(&self.part_path);
                bail!(
                    "{}",
                    ErrorCode::OutputHashMismatch.msg(format!(
                        "sha256 mismatch for {} (expected {expected}, got {digest})",
                        self.final_path.display()
                    ))
                );
            }
        }
        std::fs::rename(&self.part_path, &self.final_path)?;
        Ok(self.final_path)
    }
}

/// What the plan-based reconstruction did, for stats and `--dump-plan`.
#[derive(Default)]
struct ReconOutcome {
    plans: Vec<cavs_rebuild_plan::ReconstructionPlan>,
    sources: Option<hybrid::ExecOutcome>,
    plan: Option<cavs_rebuild_plan::PlanStats>,
    no_op_files: u64,
    no_op_bytes: u64,
}

/// v0.6.0 unified reconstruction for container payloads: every data track
/// is rebuilt by a [`cavs_rebuild_plan::ReconstructionPlan`] executed over
/// the previous artifact, the chunk cache and the network. Directory
/// payloads are staged first and committed atomically per file after all
/// hashes verify.
#[allow(clippy::too_many_arguments)]
fn reconstruct_with_plans(
    agent: &ureq::Agent,
    server: &str,
    asset: &str,
    manifest: &Manifest,
    payload_kind: &str,
    cache: &ChunkCache,
    output: &Path,
    prev: Option<&hybrid::PreviousArtifact>,
    opts: &HybridOpts,
) -> Result<(Vec<PathBuf>, ReconOutcome)> {
    std::fs::create_dir_all(output)?;
    let dir_mode = payload_kind == "directory";
    let staging_root = output.join(".cavs-staging");
    let sha_by_name: std::collections::HashMap<&str, &str> = manifest
        .meta
        .iter()
        .filter_map(|(k, v)| k.strip_prefix("sha256:").map(|n| (n, v.as_str())))
        .collect();

    let mut recon = ReconOutcome::default();
    let mut agg_sources = hybrid::ExecOutcome::default();
    let mut agg_plan = cavs_rebuild_plan::PlanStats::default();
    let mut primaries = Vec::new();
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
    let empty_index = std::collections::HashMap::new();
    let prev_index = prev.map(|p| &p.index).unwrap_or(&empty_index);

    for track in &manifest.tracks {
        if track.kind == "video" || track.kind == "audio" {
            bail!("container payload with media tracks is not supported");
        }
        if track.name.contains("..") || track.name.starts_with('/') {
            bail!("unsafe track name: {}", track.name);
        }
        let expected = sha_by_name.get(track.name.as_str()).copied();
        let final_path = output.join(&track.name);

        // No-op level 3 (directory mode): an unchanged file — including one
        // the user modified and the publisher did not touch — is left alone.
        if dir_mode && !opts.force_reconstruct {
            if let Some(exp) = expected {
                if hybrid::file_matches_sha256(&final_path, exp) {
                    recon.no_op_files += 1;
                    recon.no_op_bytes +=
                        std::fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);
                    continue;
                }
            }
        }

        let needed = hybrid::needed_chunks_for_track(manifest, track.track_id);
        let plan = cavs_rebuild_plan::plan(
            asset,
            &track.name,
            &needed,
            cavs_rebuild_plan::availability_from_sets(|h| cache.contains(h), prev_index),
        );
        let dest = if dir_mode {
            staging_root.join(&track.name)
        } else {
            final_path.clone()
        };
        let mut part = PartFile::create(dest, expected.is_some())?;
        let outcome = hybrid::execute_plan(
            &plan,
            prev,
            cache,
            |bytes| part.append_bytes(bytes),
            |hash| http_get_bytes(agent, &format!("{server}/api/assets/{asset}/chunks/{hash}")),
        )?;
        let written = part.finish(expected)?;
        if dir_mode {
            staged.push((written, final_path));
        } else if track.codec == "raw" {
            primaries.push(written);
        }

        agg_sources.previous_artifact_bytes += outcome.previous_artifact_bytes;
        agg_sources.cache_chunk_bytes += outcome.cache_chunk_bytes;
        agg_sources.demoted_chunks += outcome.demoted_chunks;
        agg_sources.repair_wire_bytes += outcome.repair_wire_bytes;
        agg_plan.ops_total += plan.stats.ops_total;
        agg_plan.ops_before_coalescing += plan.stats.ops_before_coalescing;
        agg_plan.coalesced_ops += plan.stats.coalesced_ops;
        agg_plan.copy_previous_range_ops += plan.stats.copy_previous_range_ops;
        agg_plan.copy_cache_chunk_ops += plan.stats.copy_cache_chunk_ops;
        agg_plan.fetch_chunk_ops += plan.stats.fetch_chunk_ops;
        agg_plan.previous_artifact_bytes += plan.stats.previous_artifact_bytes;
        agg_plan.cache_chunk_bytes += plan.stats.cache_chunk_bytes;
        agg_plan.network_bytes += plan.stats.network_bytes;
        agg_plan.source_selection_ms += plan.stats.source_selection_ms;
        recon.plans.push(plan);
    }

    if dir_mode {
        commit_directory(manifest, output, &staging_root, &staged, opts.prune)
            .map_err(|e| anyhow!(ErrorCode::ContainerApplyFailed.msg(format!("{e:#}"))))?;
    }
    recon.sources = Some(agg_sources);
    recon.plan = Some(agg_plan);
    Ok((primaries, recon))
}

/// Directory-mode commit: every staged file verified already, so this is
/// only renames plus metadata. Order keeps the tree usable at every step:
/// dirs first, then file moves, then symlinks/permissions, prune last.
/// The journal records intent so an interrupted apply can be diagnosed and
/// finished by simply re-running the fetch (per-file no-op detection makes
/// that cheap).
fn commit_directory(
    manifest: &Manifest,
    output: &Path,
    staging_root: &Path,
    staged: &[(PathBuf, PathBuf)],
    prune: bool,
) -> Result<()> {
    let meta_paths = |prefix: &'static str| {
        manifest
            .meta
            .iter()
            .filter_map(move |(k, v)| k.strip_prefix(prefix).map(|p| (p.to_string(), v.clone())))
            .filter(|(p, _)| !p.contains("..") && !p.starts_with('/'))
    };

    // Journal the planned moves before touching the target tree.
    if !staged.is_empty() {
        std::fs::create_dir_all(staging_root)?;
        let journal: Vec<serde_json::Value> = staged
            .iter()
            .map(|(from, to)| serde_json::json!({"from": from, "to": to}))
            .collect();
        let _ = std::fs::write(
            staging_root.join("apply-journal.json"),
            serde_json::to_vec_pretty(&serde_json::json!({"moves": journal}))?,
        );
    }

    for (dir, _) in meta_paths("dir:") {
        std::fs::create_dir_all(output.join(dir))?;
    }
    for (from, to) in staged {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(from, to).with_context(|| format!("installing {}", to.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for (path, _) in meta_paths("exec:") {
            let target = output.join(&path);
            if let Ok(meta) = std::fs::metadata(&target) {
                let mut perm = meta.permissions();
                perm.set_mode(perm.mode() | 0o755);
                let _ = std::fs::set_permissions(&target, perm);
            }
        }
        for (path, link_target) in meta_paths("symlink:") {
            let at = output.join(&path);
            if let Some(parent) = at.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let _ = std::fs::remove_file(&at);
            std::os::unix::fs::symlink(&link_target, &at)
                .with_context(|| format!("creating symlink {}", at.display()))?;
        }
    }
    #[cfg(not(unix))]
    for (path, _) in meta_paths("symlink:") {
        eprintln!("[apply] skipping symlink {path} (unsupported on this platform)");
    }

    if prune {
        let mut keep: std::collections::HashSet<PathBuf> = manifest
            .tracks
            .iter()
            .map(|t| output.join(&t.name))
            .collect();
        for (p, _) in meta_paths("symlink:") {
            keep.insert(output.join(p));
        }
        let keep_dirs: std::collections::HashSet<PathBuf> = keep
            .iter()
            .flat_map(|p| p.ancestors().map(Path::to_path_buf).collect::<Vec<_>>())
            .chain(meta_paths("dir:").map(|(d, _)| output.join(d)))
            .collect();
        prune_extraneous(output, staging_root, &keep, &keep_dirs)?;
    }

    let _ = std::fs::remove_dir_all(staging_root);
    Ok(())
}

/// Remove files (and then empty dirs) under `dir` that are not part of the
/// new container. The staging root is never touched.
fn prune_extraneous(
    dir: &Path,
    staging_root: &Path,
    keep: &std::collections::HashSet<PathBuf>,
    keep_dirs: &std::collections::HashSet<PathBuf>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path == staging_root {
            continue;
        }
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.is_dir() && !meta.file_type().is_symlink() {
            prune_extraneous(&path, staging_root, keep, keep_dirs)?;
            if !keep_dirs.contains(&path) && std::fs::read_dir(&path)?.next().is_none() {
                let _ = std::fs::remove_dir(&path);
            }
        } else if !keep.contains(&path) {
            eprintln!("[apply] pruning {}", path.display());
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(())
}

/// Mirror of `cavs unpack`, streaming from the chunk cache: per video track
/// an HLS dir + combined mp4; data tracks at their logical names, verified
/// against the manifest's per-file SHA-256 when present.
fn reconstruct_streaming(
    manifest: &Manifest,
    cache: &ChunkCache,
    output: &Path,
) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(output)?;
    let sha_by_name: std::collections::HashMap<&str, &str> = manifest
        .meta
        .iter()
        .filter_map(|(k, v)| k.strip_prefix("sha256:").map(|n| (n, v.as_str())))
        .collect();
    let mut primaries = Vec::new();

    for track in &manifest.tracks {
        let mut segs: Vec<_> = manifest
            .segments
            .iter()
            .filter(|s| s.track_id == track.track_id)
            .collect();
        segs.sort_by_key(|s| (s.pts_start, s.segment_id));

        match track.kind.as_str() {
            "video" | "audio" => {
                let dir = output.join(&track.name);
                let mut init = PartFile::create(dir.join("init.mp4"), false)?;
                let mut combined =
                    PartFile::create(output.join(format!("{}.mp4", track.name)), false)?;
                for c in &track.init_chunks {
                    init.append_chunk(cache, &c.hash)?;
                    combined.append_chunk(cache, &c.hash)?;
                }
                init.finish(None)?;
                for (ordinal, seg) in segs.iter().enumerate() {
                    let mut part =
                        PartFile::create(dir.join(format!("seg_{ordinal:05}.m4s")), false)?;
                    for c in &seg.chunks {
                        part.append_chunk(cache, &c.hash)?;
                        combined.append_chunk(cache, &c.hash)?;
                    }
                    part.finish(None)?;
                }
                primaries.push(combined.finish(None)?);
            }
            _ => {
                if track.name.contains("..") || track.name.starts_with('/') {
                    bail!("unsafe track name: {}", track.name);
                }
                let expected = sha_by_name.get(track.name.as_str()).copied();
                let mut part = PartFile::create(output.join(&track.name), expected.is_some())?;
                for seg in &segs {
                    for c in &seg.chunks {
                        part.append_chunk(cache, &c.hash)?;
                    }
                }
                let path = part.finish(expected)?;
                if track.codec == "raw" {
                    primaries.push(path);
                }
            }
        }
    }
    Ok(primaries)
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

// ---------------------------------------------------------------------------
// Minimal HTTP helpers (plain HTTP origin for the MVP; TLS via rustls is the
// planned evolution).
// ---------------------------------------------------------------------------

fn http_get_string(agent: &ureq::Agent, url: &str) -> Result<String> {
    retry::with_retry(&format!("GET {url}"), || agent.get(url).call())?
        .into_string()
        .context("reading response body")
}

/// GET the asset manifest asking for the compact binary v2 format, with
/// JSON v1 as the negotiated fallback (v0.2.x servers ignore Accept and
/// always answer JSON — both parse through `cavs_manifest::read_manifest`).
fn http_get_manifest(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let resp = retry::with_retry(&format!("GET {url}"), || {
        agent
            .get(url)
            .set(
                "accept",
                &format!(
                    "{}, application/json;q=0.5",
                    cavs_manifest::MANIFEST_V2_CONTENT_TYPE
                ),
            )
            .call()
    })?;
    let mut out = Vec::new();
    resp.into_reader()
        .read_to_end(&mut out)
        .context("reading manifest body")?;
    Ok(out)
}

fn http_get_bytes(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let resp = retry::with_retry(&format!("GET {url}"), || agent.get(url).call())?;
    let mut out = Vec::new();
    resp.into_reader()
        .read_to_end(&mut out)
        .context("reading chunk body")?;
    Ok(out)
}

fn http_post_json(agent: &ureq::Agent, url: &str, body: &str) -> Result<String> {
    retry::with_retry(&format!("POST {url}"), || {
        agent
            .post(url)
            .set("content-type", "application/json")
            .send_string(body)
    })?
    .into_string()
    .context("reading response body")
}

/// POST returning the body as a reader: large batches are consumed as a
/// stream (peak RAM = one chunk) instead of being buffered whole. Retries
/// cover request establishment; a failure mid-stream surfaces to the
/// caller, and a re-run resumes from the chunk cache.
fn http_post_reader(
    agent: &ureq::Agent,
    url: &str,
    body: &str,
) -> Result<Box<dyn std::io::Read + Send + Sync + 'static>> {
    let resp = retry::with_retry(&format!("POST {url}"), || {
        agent
            .post(url)
            .set("content-type", "application/json")
            .send_string(body)
    })?;
    Ok(resp.into_reader())
}
