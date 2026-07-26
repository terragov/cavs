//! `cavs` — CLI for the CAVS-1 content-addressable video packaging format.
//!
//! Converts videos into `.cavs` (via ffmpeg CMAF/fMP4 segmentation),
//! reconstructs them back to playable MP4/HLS, inspects, verifies and plays.

mod analyze_packs;
mod apply_cmd;
mod bench_butler;
mod bench_butler_full;
mod bench_compression;
mod bench_delta;
mod bench_env;
mod bench_pairwise;
mod bench_pipeline;
mod bench_routes;
mod bench_steampipe_cases;
mod bench_versions;
mod blob_detect;
mod build_cmd;
mod certify;
mod classify;
mod compare;
mod corrupt;
mod diff_plan;
mod doctor;
mod ffmpeg;
mod ignore;
mod inspect_cmd;
mod io_estimate;
mod manifest_cmd;
mod optimize_layout;
mod optimize_patch;
mod pack;
mod pack_dir;
mod patch_policy;
mod patch_policy_bench;
mod patch_v2;
mod plan_update;
mod preview;
mod profile;
mod publish_dir;
mod publish_preview;
mod report;
mod route_plan;
mod serve_cmd;
mod signature_cmd;
mod steampipe_cmd;
mod store;
mod sweep;
mod synth;
mod test_recovery;
mod tool_metrics;
mod unpack;
mod verify_install;
mod workspace_cmd;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "cavs",
    version,
    about = "CAVS — content-addressable, deduplicated packaging",
    long_about = "CAVS packages files, large builds or video into .cavs: deduplicated \
                  FastCDC chunks, zstd-compressed and verifiable (BLAKE3 + Merkle + \
                  optional Ed25519 signature). Served by cavs-server, a client with a \
                  cache downloads only the bytes it doesn't already have.",
    after_help = "EXAMPLES:\n  \
        cavs pack --raw build_v42.bin -o v42.cavs           # a release artifact\n  \
        cavs pack --raw --sign-key pub.key data/* -o r.cavs # signed\n  \
        cavs pack movie.mp4 -o movie.cavs                   # video (segmented via ffmpeg)\n  \
        cavs info v42.cavs                                  # structure and dedupe\n  \
        cavs verify v42.cavs --pubkey pub.key.pub           # integrity + signature\n  \
        cavs unpack v42.cavs -o restored/                   # exact reconstruction\n\n\
        To serve and update clients: cavs-server / cavs-client --help"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ChunkModeArg {
    /// Fixed 256 KiB chunks (default for packaged media segments).
    Fixed,
    /// FastCDC 64/256/1024 KiB (default for raw assets).
    Cdc,
    /// Aggressive FastCDC 16/64/256 KiB (screen content, very repetitive data).
    Screen,
}

impl ChunkModeArg {
    pub fn to_mode(self, chunk_size: Option<usize>) -> cavs_chunker::ChunkMode {
        use cavs_chunker::ChunkMode;
        match self {
            ChunkModeArg::Fixed => ChunkMode::Fixed {
                size: chunk_size.unwrap_or(256 * 1024),
            },
            ChunkModeArg::Cdc => match chunk_size {
                Some(avg) => ChunkMode::Cdc {
                    min: (avg / 4).max(1024),
                    avg,
                    max: avg * 4,
                    norm: cavs_chunker::NORM_DEFAULT,
                },
                None => ChunkMode::asset_default(),
            },
            ChunkModeArg::Screen => ChunkMode::screen_default(),
        }
    }
}

#[derive(Subcommand)]
// One Command is parsed per process; the size spread from flag-heavy
// variants (certify) is irrelevant.
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Package files (--raw) or videos into a deduplicated .cavs.
    ///
    /// With --raw it accepts any file (PCKs, bundles, binaries) and uses
    /// FastCDC 64 KiB + zstd 3 (the configuration validated in benchmarks).
    /// Without --raw it treats inputs as video: ffmpeg segments them into
    /// CMAF/fMP4 and CAVS packages the segments.
    Pack {
        /// Input video files (or arbitrary files with --raw).
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        /// Output .cavs path.
        #[arg(short, long)]
        output: PathBuf,
        /// Pack raw file bytes without ffmpeg segmentation (any file type).
        #[arg(long)]
        raw: bool,
        /// Target media segment duration in seconds (video mode).
        #[arg(long, default_value_t = 4.0)]
        segment_time: f64,
        /// Chunking strategy for media/asset payloads.
        #[arg(long, value_enum)]
        mode: Option<ChunkModeArg>,
        /// Chunk size in bytes (fixed size, or CDC average).
        #[arg(long)]
        chunk_size: Option<usize>,
        /// Chunk profile: `auto` classifies the payload and sweeps candidate
        /// profiles by cost, or force one of fixed-256k/fixed-512k/fixed-1m/
        /// fastcdc-16k/fastcdc-32k/fastcdc-64k/fastcdc-128k/fastcdc-256k.
        /// Overrides --mode.
        #[arg(long)]
        profile: Option<String>,
        /// Previous version of the (single) input, so `--profile auto`
        /// optimises for update egress instead of first install.
        #[arg(long)]
        prev: Option<PathBuf>,
        /// Also write a full bootstrap artifact (`<output>.bootstrap.zst`):
        /// the whole input zstd-compressed, so cache-less clients can install
        /// at full-artifact cost and seed their cache locally (raw mode,
        /// single input).
        #[arg(long)]
        bootstrap: bool,
        /// Disable zstd compression of stored chunks.
        #[arg(long)]
        no_compress: bool,
        /// zstd level for chunk storage/wire compression.
        #[arg(long, default_value_t = 3)]
        zstd_level: i32,
        /// Force re-encode (H.264/AAC) instead of trying stream copy first.
        #[arg(long)]
        transcode: bool,
        /// Sign the packed content with this Ed25519 secret key file
        /// (as produced by `cavs keygen`).
        #[arg(long)]
        sign_key: Option<PathBuf>,
        /// Report reusable bytes against a previous version's `.cavssig`
        /// (v0.6.0 hybrid reconstruction; raw mode).
        #[arg(long)]
        against_signature: Option<PathBuf>,
    },
    /// Package a directory tree as a container asset (v0.6.0 preview):
    /// one deduplicated data track per file, plus directory/symlink/exec
    /// metadata. Clients apply it with per-file no-op detection and staged
    /// installs.
    PackDir {
        /// The directory to package.
        input: PathBuf,
        /// Output .cavs path.
        #[arg(short, long)]
        output: PathBuf,
        /// Chunk profile label (fixed-256k/…/fastcdc-64k…); default (and
        /// `auto`) is the update-validated fastcdc-64k.
        #[arg(long)]
        profile: Option<String>,
        /// Disable zstd compression of stored chunks.
        #[arg(long)]
        no_compress: bool,
        /// zstd level for chunk storage/wire compression.
        #[arg(long, default_value_t = 3)]
        zstd_level: i32,
        /// Sign the packed content with this Ed25519 secret key file.
        #[arg(long)]
        sign_key: Option<PathBuf>,
        /// Exclude entries matching this glob (repeatable; merged with the
        /// tree root's `.cavsignore`). `*`/`?` stay in one segment, `**`
        /// crosses, a trailing `/` ignores a whole directory.
        #[arg(long)]
        ignore: Vec<String>,
    },
    /// Export, inspect, list and verify compact `.cavssig` signatures:
    /// the old version's layout and block hashes, so new versions can be
    /// planned against it without the old bytes.
    Signature {
        #[command(subcommand)]
        action: SignatureAction,
    },
    /// Compare a new build against a previous version's `.cavssig`:
    /// NEW/MODIFIED/DELETED/SAME per entry, estimated update sizes per
    /// route, and warnings for patch-hostile (compressed) files.
    Preview {
        /// The new build (directory or single artifact).
        new_build: PathBuf,
        /// The previous version's `.cavssig`.
        #[arg(long)]
        against: PathBuf,
        /// Only print entries that changed.
        #[arg(long)]
        changes_only: bool,
        /// Flag archive/high-entropy files (zip, gzip, zstd, 7z, …) whose
        /// shape defeats block-level patching, with cost estimates.
        #[arg(long)]
        detect_compressed_blobs: bool,
        /// Machine-readable JSON on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Produce a deterministic offline reconstruction plan (`.cavsplan`)
    /// describing how to rebuild the new build from the old one.
    DiffPlan {
        /// The old build (file or directory). Optional with --old-signature.
        old: Option<PathBuf>,
        /// The new build (file or directory).
        new: PathBuf,
        /// Output `.cavsplan` path.
        #[arg(short, long)]
        out: PathBuf,
        /// Diff against a `.cavssig` instead of the old bytes.
        #[arg(long)]
        old_signature: Option<PathBuf>,
        /// Emit an analysis-only plan (ops and estimates, no payload).
        #[arg(long)]
        analysis: bool,
        /// Signature block size in KiB when signing the old build here.
        #[arg(long, default_value_t = 64)]
        block_kib: u32,
        /// zstd level for the plan's inline payload.
        #[arg(long, default_value_t = 19)]
        zstd_level: i32,
        /// Also write a human-readable Markdown report.
        #[arg(long)]
        report: Option<PathBuf>,
    },
    /// Apply a `.cavsplan` locally: artifact plans write `<out>.part` then
    /// rename; directory plans stage, verify, journal and commit per file.
    /// A failed apply never leaves corrupt output.
    Apply {
        /// The old build (file or directory).
        #[arg(long)]
        old: Option<PathBuf>,
        /// The `.cavsplan` to execute.
        #[arg(long)]
        plan: Option<PathBuf>,
        /// Output path (omit with --inplace).
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Update the old install in place (directory plans).
        #[arg(long)]
        inplace: bool,
        /// Re-verify every output hash after the apply commits.
        #[arg(long)]
        verify: bool,
        /// Delete files the plan marks as removed (managed deletions).
        #[arg(long)]
        delete_removed_files: bool,
        /// Verify the old source against the plan's recorded hash first.
        #[arg(long)]
        check_old: bool,
        /// Resume an interrupted directory apply from its journal.
        #[arg(long)]
        resume: Option<PathBuf>,
        /// Machine-readable JSON stats on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Verify an installed artifact or directory against a `.cavssig` or a
    /// manifest; reports MODIFIED/MISSING/EXTRA and exits non-zero on
    /// mismatch.
    VerifyInstall {
        /// The installed build (file or directory).
        target: PathBuf,
        /// Verify against this `.cavssig`.
        #[arg(long)]
        signature: Option<PathBuf>,
        /// Verify against this manifest's recorded SHA-256 digests.
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// Tolerate files not covered by the signature (mods, saves).
        #[arg(long)]
        allow_extra_files: bool,
        /// Machine-readable JSON on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Identify any CAVS file (.cavs, .cavssig, .cavsplan, .cavspatch,
    /// manifest, bootstrap) and print its headline facts.
    File {
        input: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// List the entries inside a CAVS file (signatures, plans, containers,
    /// manifests).
    Ls {
        input: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Generate an optimized pairwise sidecar (`.cavspatch` v2): per-file
    /// strategy selection (copy-old/renames, CAVS plan ops, bsdiff,
    /// xdelta3, recompressed full data), every candidate measured, the
    /// smallest kept. Serves exactly one old→new pair — generate only for
    /// hot pairs (`cavs patch-policy`); the pair count grows O(N²).
    OptimizePatch {
        /// Old build (file or directory).
        #[arg(long)]
        old: PathBuf,
        /// New build (same kind as --old).
        #[arg(long)]
        new: PathBuf,
        /// auto (measure candidates per file) | plan | bsdiff | xdelta3 | full.
        #[arg(long, default_value = "auto")]
        algo: String,
        /// auto (best of zstd-19/brotli-9 per section) | zstd-N | brotli-N | none.
        #[arg(long, default_value = "auto")]
        compression: String,
        /// Write a per-file strategy report (Markdown) to this path.
        #[arg(long)]
        explain_strategies: Option<PathBuf>,
        /// Output `.cavspatch` path.
        #[arg(short, long)]
        out: PathBuf,
    },
    /// Apply a `.cavspatch` sidecar (v1 or v2): staged, journaled,
    /// hash-verified; nothing is committed on any mismatch.
    ApplyPatch {
        /// Old build (must match what the patch was generated against).
        #[arg(long)]
        old: PathBuf,
        /// The `.cavspatch` file.
        #[arg(long)]
        patch: PathBuf,
        /// Output path (file for artifact patches, directory for directory
        /// patches; may equal --old for in-place).
        #[arg(short, long)]
        out: PathBuf,
        /// Refuse strategies whose estimated peak memory exceeds this
        /// budget (e.g. 128MiB); the .cavsplan route always fits small
        /// budgets.
        #[arg(long)]
        memory_budget: Option<String>,
        /// Delete files the patch marks as removed (managed deletions).
        #[arg(long)]
        delete_removed_files: bool,
        /// Verify old files against their recorded hashes before use.
        #[arg(long)]
        check_old: bool,
    },
    /// Choose the best delivery route for one client state: no-op,
    /// chunks/hybrid, offline plan, optimized sidecar, bootstrap or full
    /// download — scored under a device profile.
    RoutePlan {
        /// The installed old version (omit for a fresh install).
        #[arg(long)]
        installed: Option<PathBuf>,
        /// The target build (file or directory).
        #[arg(long)]
        new: PathBuf,
        /// A pre-generated `.cavsplan` for this pair (exact size).
        #[arg(long)]
        plan: Option<PathBuf>,
        /// A pre-generated `.cavspatch` for this pair (exact size).
        #[arg(long)]
        patch: Option<PathBuf>,
        /// A bootstrap artifact for the target (exact size).
        #[arg(long)]
        bootstrap: Option<PathBuf>,
        /// Device profile the routes are scored under.
        #[arg(long, value_enum, default_value_t = route_plan::ClientProfile::Default)]
        profile: route_plan::ClientProfile,
        /// Machine-readable JSON on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Sidecar hot-pair planning plus the patch graph tools (v1.1.0):
    /// without a subcommand, decide which old→new pairs deserve an
    /// optimized sidecar (previous, latest-stable, top-installed, pins).
    /// Subcommands: `graph` (build a policy patch graph), `simulate`
    /// (replay a traffic model on a measured graph), `explain` (show one
    /// path).
    PatchPolicy {
        #[command(subcommand)]
        action: Option<PatchPolicyAction>,
        /// Ordered, comma-separated version list; the last one is the
        /// release target (e.g. v1,v2,v3-beta,v4).
        #[arg(long)]
        versions: Option<String>,
        /// JSON map of installed shares, e.g. {"v1":0.12,"v3":0.55}.
        #[arg(long)]
        distribution: Option<PathBuf>,
        /// TOML policy file ([optimized_patches] table); defaults apply
        /// without one.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Machine-readable JSON on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Publish a directory build in one pass: container + signature +
    /// offline plan + optimized sidecar vs the previous release, with a
    /// preview (renames, compressed-blob warnings) first.
    PublishDir {
        /// The exported build folder.
        build: PathBuf,
        /// The previous build directory or its `.cavssig`.
        #[arg(long)]
        previous: Option<PathBuf>,
        /// Where the release files are written.
        #[arg(long)]
        out_dir: PathBuf,
        /// auto (generate the previous→this sidecar) | off.
        #[arg(long, default_value = "auto")]
        optimize_patches: String,
        /// Exclude entries matching this glob (repeatable; merged with
        /// `.cavsignore`).
        #[arg(long)]
        ignore: Vec<String>,
        /// zstd level for the container's stored chunks.
        #[arg(long, default_value_t = 3)]
        zstd_level: i32,
        /// Sign the packed content with this Ed25519 secret key file.
        #[arg(long)]
        sign_key: Option<PathBuf>,
        /// Only print the preview; write nothing.
        #[arg(long)]
        preview: bool,
    },
    /// Measure candidate chunk profiles on a payload (optionally against its
    /// previous version) and report the cheapest per cost model.
    Sweep {
        /// The payload to analyse (e.g. the new build).
        input: PathBuf,
        /// Previous version of the payload, to measure real chunk reuse.
        #[arg(long)]
        prev: Option<PathBuf>,
        /// Comma-separated profiles to test (default: recommended by the
        /// payload classifier).
        #[arg(long)]
        profiles: Option<String>,
        /// zstd level assumed for storage estimates.
        #[arg(long, default_value_t = 3)]
        zstd_level: i32,
        /// Write the full estimates as JSON to this path.
        #[arg(long)]
        json: Option<PathBuf>,
    },
    /// Reconstruct the original media from a .cavs file.
    Unpack {
        input: PathBuf,
        /// Output directory.
        #[arg(short, long)]
        output: PathBuf,
        /// Skip writing the combined progressive .mp4 per video track.
        #[arg(long)]
        no_mp4: bool,
    },
    /// Show structure, dedup and compression statistics of a .cavs file.
    Info {
        input: PathBuf,
        /// Also list every segment.
        #[arg(long)]
        segments: bool,
        /// Also list every chunk.
        #[arg(long)]
        chunks: bool,
    },
    /// Verify every chunk hash, the Merkle root and all section hashes.
    Verify {
        input: PathBuf,
        /// Additionally require a valid content signature from this Ed25519
        /// public key (64 hex chars, or a path to a .pub file).
        #[arg(long)]
        pubkey: Option<String>,
    },
    /// Generate an Ed25519 signing keypair: <output> (secret, hex) and
    /// <output>.pub (public key, hex).
    Keygen {
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Reconstruct to a temp dir and play with ffplay.
    Play { input: PathBuf },
    /// Manage a global content-addressable store: ingest releases dedup'd
    /// across all versions/titles, unpublish, garbage collect.
    Store {
        /// Store directory (created if missing).
        dir: PathBuf,
        #[command(subcommand)]
        action: StoreAction,
    },
    /// Inspect manifest formats: export readable JSON, benchmark
    /// json-v1 vs binary-v2 (v0.3.0 compact manifest).
    Manifest {
        #[command(subcommand)]
        action: ManifestAction,
    },
    /// Diagnose a deployment (v0.5.0): container integrity, manifest
    /// encodability, bootstrap sidecar, store/pack consistency, cache
    /// health. Read-only; exits non-zero on problems.
    Doctor {
        /// A .cavs file to check.
        input: Option<PathBuf>,
        /// A global store directory to check.
        #[arg(long)]
        store: Option<PathBuf>,
        /// A client chunk-cache directory to check.
        #[arg(long)]
        cache: Option<PathBuf>,
    },
    /// Hardening test harnesses (v0.5.0).
    Test {
        #[command(subcommand)]
        action: TestAction,
    },
    /// Synthetic large-build benchmarks (v0.5.0): generate deterministic
    /// datasets and measure the whole pipeline on them.
    Bench {
        #[command(subcommand)]
        action: BenchAction,
    },
    /// SteamPipe-style build diagnostics (v0.9.0): explain why an update
    /// is expensive under a fixed-1MiB public model and how to fix the
    /// layout. Estimates only — never Valve's exact implementation.
    Analyze {
        #[command(subcommand)]
        action: AnalyzeAction,
    },
    /// Analyze pack files across two builds (v0.9.0): change heatmaps,
    /// scatteredness, TOC churn, compressed blobs, size advisories.
    AnalyzePacks {
        /// Old build (directory or single pack file).
        old: PathBuf,
        /// New build (same kind as old).
        new: PathBuf,
        /// Engine hint: auto|generic|unreal|unity|godot.
        #[arg(long, default_value = "auto")]
        engine: String,
        /// Write the report as Markdown to this path.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Machine-readable JSON on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Estimate the local disk I/O cost of an update per delivery route
    /// (v0.9.0): reads, writes, temp disk and per-device time estimates.
    IoEstimate {
        /// Old build (file or directory).
        old: PathBuf,
        /// New build (same kind as old).
        new: PathBuf,
        /// TOML file of device profiles (defaults: hdd, sata_ssd, nvme).
        #[arg(long)]
        device_profiles: Option<PathBuf>,
        /// Write the report as Markdown to this path.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Machine-readable JSON on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Release-readiness report (v0.9.0): measure every delivery route
    /// for a build against its predecessor, add the SteamPipe-style
    /// estimate, flag layout problems and recommend a route.
    PublishPreview {
        /// The new build (directory or artifact). Omit in workspace mode.
        build: Option<PathBuf>,
        /// The previous build (same kind).
        #[arg(long)]
        previous: Option<PathBuf>,
        /// Workspace mode: resolve builds from this workspace instead.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// App id inside the workspace (default: the workspace default).
        #[arg(long)]
        app: Option<String>,
        /// Source build id (workspace mode), e.g. build_1001.
        #[arg(long)]
        from: Option<String>,
        /// Target build id (workspace mode).
        #[arg(long)]
        to: Option<String>,
        /// Routes to include: all (default) also probes butler/pairwise
        /// proxies when their tools are installed.
        #[arg(long, default_value = "all")]
        routes: String,
        /// Path to the butler binary (default: `butler` on PATH).
        #[arg(long)]
        butler_bin: Option<String>,
        /// Results directory (preview.md + preview.json + route artifacts).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Machine-readable JSON on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Choose the best delivery route under a policy (v0.9.0):
    /// network/CPU/RAM/disk-weighted scoring per client state.
    PlanUpdate {
        /// Installed old version (omit for a fresh install).
        #[arg(long)]
        from: Option<PathBuf>,
        /// Target version.
        #[arg(long)]
        to: PathBuf,
        /// Pre-generated `.cavsplan` for this pair (exact size).
        #[arg(long)]
        plan: Option<PathBuf>,
        /// Pre-generated `.cavspatch` for this pair (exact size).
        #[arg(long)]
        patch: Option<PathBuf>,
        /// Bootstrap artifact for the target (exact size).
        #[arg(long)]
        bootstrap: Option<PathBuf>,
        /// Comma-separated client state: warm-cache, cold-cache,
        /// has-previous-install, cold-install, low-ram, low-disk,
        /// slow-hdd, fast-nvme.
        #[arg(long, default_value = "")]
        client_state: String,
        /// Policy: network_min|cpu_min|ram_min|disk_io_min|balanced|
        /// hdd_friendly|developer_fast.
        #[arg(long, default_value = "balanced")]
        policy: String,
        /// Machine-readable JSON on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Advisory build-layout recommendations for patch-efficient output
    /// (v0.9.0). Never modifies files.
    OptimizeLayout {
        /// Old build.
        old: PathBuf,
        /// New build.
        new: PathBuf,
        /// Engine hint: auto|generic|unreal|unity|godot.
        #[arg(long, default_value = "auto")]
        engine: String,
        /// Write the plan as Markdown to this path.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Write the plan as JSON for future automation.
        #[arg(long)]
        write_plan: Option<PathBuf>,
        /// Machine-readable JSON on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Local app/depot/branch/build workspace (v0.9.0): SteamPipe-like
    /// distribution concepts as local metadata, no platform required.
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
    /// Manage depots in a workspace app and analyze content sharing.
    Depot {
        #[command(subcommand)]
        action: DepotAction,
    },
    /// Manage branches: add, promote, rollback, promote-preview.
    Branch {
        #[command(subcommand)]
        action: BranchAction,
    },
    /// Record builds in a workspace; sign, verify, encrypt or decrypt
    /// release artifacts (local authenticity — not DRM).
    Build {
        #[command(subcommand)]
        action: BuildAction,
    },
    /// Simulate what a client downloads for an install or update, by
    /// platform, language and ownership (v0.9.0).
    InstallPlan {
        /// Workspace directory.
        #[arg(long, default_value = "./cavs-workspace")]
        workspace: PathBuf,
        /// App id (default: the workspace default app).
        #[arg(long)]
        app: Option<String>,
        /// Branch whose current build is the target.
        #[arg(long)]
        branch: Option<String>,
        /// Player platform: windows|linux|macos.
        #[arg(long)]
        platform: Option<String>,
        /// Player language, e.g. es.
        #[arg(long)]
        language: Option<String>,
        /// Comma-separated owned depot ids (e.g. base,dlc1,lang-es).
        #[arg(long, default_value = "")]
        owned: String,
        /// Installed build id (omit for a fresh install).
        #[arg(long)]
        from: Option<String>,
        /// Target build id (overrides --branch).
        #[arg(long)]
        to: Option<String>,
        /// Write the plan as Markdown to this path.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Machine-readable JSON on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Release-readiness certification (v1.0.0): integrity, byte-identical
    /// reconstruction, route selection per client state, regression guard,
    /// Godot PCK flow, workspace install plans and reproducible reports —
    /// one command, stable exit codes (0 pass, 1 fail, 2 warnings,
    /// 3 missing dependency, 4 invalid input, 5 internal error).
    Certify {
        #[command(subcommand)]
        action: Option<certify::CertifyAction>,
        #[command(flatten)]
        full: certify::FullArgs,
    },
    /// Local development content server over a workspace (v0.9.0).
    /// No auth, plain HTTP — never production.
    Serve {
        /// Workspace directory.
        workspace: PathBuf,
        /// App id (default: the workspace default app).
        #[arg(long)]
        app: Option<String>,
        /// Branch used in the startup banner (informational).
        #[arg(long)]
        branch: Option<String>,
        /// Directory of published release files, served under
        /// /api/assets/{asset}/{file}.
        #[arg(long)]
        releases: Option<PathBuf>,
        /// Port to listen on (127.0.0.1 only).
        #[arg(long, default_value_t = 8990)]
        port: u16,
    },
}

#[derive(Subcommand)]
enum AnalyzeAction {
    /// Diagnose a build transition under the SteamPipe-style fixed-1MiB
    /// model: scattered churn, shuffling, TOC churn, compressed blobs,
    /// metadata churn — with recommendations.
    Steampipe {
        /// Old build (file or directory).
        old: PathBuf,
        /// New build (same kind as old).
        new: PathBuf,
        /// Engine hint: auto|generic|unreal|unity|godot.
        #[arg(long, default_value = "auto")]
        engine: String,
        /// Exclude entries matching this glob (repeatable; merged with
        /// `.cavsignore`).
        #[arg(long)]
        ignore: Vec<String>,
        /// Write the report to this path (.md, or .json by extension).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Machine-readable JSON on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Alias of `cavs bench steampipe-style`: numbers only, no diagnosis.
    UpdateCost {
        /// Old build (file or directory).
        old: PathBuf,
        /// New build (same kind as old).
        new: PathBuf,
        /// Update model to estimate (only steampipe-style today).
        #[arg(long, default_value = "steampipe-style")]
        model: String,
        /// Machine-readable JSON on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Godot-specific PCK analysis: byte-level report, and when the PCK
    /// directory is parseable, the resource paths behind each change.
    GodotPck {
        /// Old .pck file.
        old: PathBuf,
        /// New .pck file.
        new: PathBuf,
        /// Write the report as Markdown to this path.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Machine-readable JSON on stdout.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum WorkspaceAction {
    /// Create a workspace with one app.
    Init {
        /// Workspace directory (created).
        path: PathBuf,
        /// App id, e.g. my-app.
        #[arg(long)]
        app: String,
    },
}

#[derive(Subcommand)]
enum DepotAction {
    /// Add a depot to an app.
    Add {
        /// Depot id, e.g. base, windows, hd-textures, lang-es.
        id: String,
        /// Human-readable name (default: the id).
        #[arg(long)]
        name: Option<String>,
        /// Platform filter: windows|linux|macos.
        #[arg(long)]
        platform: Option<String>,
        /// Language filter, e.g. es.
        #[arg(long)]
        language: Option<String>,
        /// Players only get this depot when they own it.
        #[arg(long)]
        optional: bool,
        #[arg(long, default_value = "./cavs-workspace")]
        workspace: PathBuf,
        #[arg(long)]
        app: Option<String>,
    },
    /// Compute shared bytes across every depot pair of a build.
    AnalyzeSharing {
        #[arg(long, default_value = "./cavs-workspace")]
        workspace: PathBuf,
        #[arg(long)]
        app: Option<String>,
        /// Build id (default: the latest build).
        #[arg(long)]
        build: Option<String>,
        /// Write the report as Markdown to this path.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Machine-readable JSON on stdout.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum BranchAction {
    /// Add a branch to an app.
    Add {
        /// Branch id, e.g. public, beta, nightly.
        id: String,
        /// Human-readable name (default: the id).
        #[arg(long)]
        name: Option<String>,
        /// Hide from default listings (local metadata only).
        #[arg(long)]
        private: bool,
        #[arg(long, default_value = "./cavs-workspace")]
        workspace: PathBuf,
        #[arg(long)]
        app: Option<String>,
    },
    /// Point a branch at a build (atomic local metadata change).
    Promote {
        #[arg(long, default_value = "./cavs-workspace")]
        workspace: PathBuf,
        #[arg(long)]
        app: Option<String>,
        #[arg(long)]
        branch: String,
        #[arg(long)]
        build: String,
    },
    /// Preview the per-depot update cost of promoting a build.
    PromotePreview {
        #[arg(long, default_value = "./cavs-workspace")]
        workspace: PathBuf,
        #[arg(long)]
        app: Option<String>,
        #[arg(long)]
        branch: String,
        #[arg(long)]
        build: String,
        /// Machine-readable JSON on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Re-point a branch at an earlier build it served before.
    Rollback {
        #[arg(long, default_value = "./cavs-workspace")]
        workspace: PathBuf,
        #[arg(long)]
        app: Option<String>,
        #[arg(long)]
        branch: String,
        #[arg(long)]
        to: String,
    },
}

#[derive(Subcommand)]
enum BuildAction {
    /// Record a build from depot source directories.
    Create {
        #[arg(long, default_value = "./cavs-workspace")]
        workspace: PathBuf,
        #[arg(long)]
        app: Option<String>,
        /// Branch to point at the new build.
        #[arg(long)]
        branch: Option<String>,
        /// Depot content as id=path (repeatable).
        #[arg(long)]
        depot: Vec<String>,
        /// Human-readable label, e.g. build_1001 or v1.2.3.
        #[arg(long)]
        label: Option<String>,
    },
    /// Sign an artifact (build, manifest, plan) with an Ed25519 key.
    Sign {
        /// The artifact to sign.
        artifact: PathBuf,
        /// Ed25519 secret key file (from `cavs keygen`).
        #[arg(long)]
        key: PathBuf,
        /// Signature output (default: <artifact>.sig).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Verify an artifact against its detached signature.
    Verify {
        /// The artifact to verify.
        artifact: PathBuf,
        /// Ed25519 public key file (.pub from `cavs keygen`).
        #[arg(long = "pub", value_name = "PUBKEY")]
        pubkey: PathBuf,
        /// Signature file (default: <artifact>.sig).
        #[arg(long)]
        sig: Option<PathBuf>,
    },
    /// Encrypt an artifact for local storage/transport (XChaCha20-
    /// Poly1305). Optional, and not DRM.
    Encrypt {
        /// The artifact to encrypt.
        artifact: PathBuf,
        /// 32-byte hex content key (`cavs build content-key`).
        #[arg(long)]
        key: PathBuf,
        /// Encrypted output path.
        #[arg(long)]
        out: PathBuf,
    },
    /// Decrypt a CAVS-encrypted artifact.
    Decrypt {
        /// The encrypted artifact.
        artifact: PathBuf,
        /// The content key it was encrypted with.
        #[arg(long)]
        key: PathBuf,
        /// Decrypted output path.
        #[arg(long)]
        out: PathBuf,
    },
    /// Generate a random 32-byte content key for encrypt/decrypt.
    ContentKey {
        /// Key output path.
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Subcommand)]
enum SignatureAction {
    /// Export a `.cavssig` from a `.cavs` container (default) or a raw
    /// file/directory (--raw).
    Export {
        /// A .cavs file, or with --raw any file or directory.
        input: PathBuf,
        /// Treat the input as a raw artifact/directory instead of a .cavs.
        #[arg(long)]
        raw: bool,
        /// Block size in KiB (64 is the empirical sweet spot for delta scanning).
        #[arg(long, default_value_t = 64)]
        block_kib: u32,
        /// Output .cavssig path.
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Print a signature's layout, chunk profile and hash counts.
    Inspect {
        input: PathBuf,
        /// Machine-readable JSON on stdout.
        #[arg(long)]
        json: bool,
    },
    /// List every entry recorded in a signature.
    Ls {
        input: PathBuf,
        /// Machine-readable JSON on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Recompute every block hash of a source and compare.
    Verify {
        input: PathBuf,
        /// The artifact or directory the signature claims to describe.
        #[arg(long)]
        against: PathBuf,
    },
}

#[derive(Subcommand)]
enum TestAction {
    /// Corruption matrix: mutate a copy of the .cavs (and its manifest,
    /// packfile and bootstrap forms) byte by byte and assert every decoder
    /// rejects the corrupt artifact cleanly.
    Corrupt {
        /// The .cavs file to mutate (a pristine copy is used per test).
        input: PathBuf,
        /// Write the matrix report as JSON to this path.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Interrupted-apply matrix (v0.8.0): SIGKILL real `cavs apply` runs
    /// at ramping delays, assert no torn files, prove journaled resume;
    /// plus corrupt-plan / corrupt-old / garbage-staging cases.
    ApplyRecovery {
        /// Old build directory.
        #[arg(long)]
        old: PathBuf,
        /// New build directory.
        #[arg(long)]
        new: PathBuf,
        /// Write apply-recovery.json here.
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum BenchAction {
    /// Generate a deterministic synthetic dataset: a base build plus
    /// update variants (small/medium/large change, shifted, reordered).
    Gen {
        /// Output dataset directory.
        #[arg(long)]
        out: PathBuf,
        /// Base build size, e.g. 100MiB, 1GiB.
        #[arg(long, default_value = "100MiB")]
        size: String,
        /// PRNG seed (same seed + size => identical dataset).
        #[arg(long, default_value_t = 5)]
        seed: u64,
    },
    /// Pack and measure every version in a dataset directory: pack time,
    /// manifest sizes, dedup, update egress, packfile counts. Writes
    /// summary.md and summary.json.
    Suite {
        /// Dataset directory produced by `cavs bench gen`.
        #[arg(long)]
        dataset: PathBuf,
        /// Results directory.
        #[arg(long)]
        out: PathBuf,
    },
    /// Compare CAVS against a block-based delta patching model on a real
    /// old/new pair (v0.6.0). Uses xdelta3/bsdiff too when on PATH.
    Delta {
        /// Old version (file or directory).
        #[arg(long)]
        old: PathBuf,
        /// New version (file or directory — same kind as --old).
        #[arg(long)]
        new: PathBuf,
        /// Directory for delta-comparison.{json,md}.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Measure compression algorithms on a payload (v0.6.0): zstd always,
    /// Brotli with `--features brotli-bench`. Defaults stay zstd-3.
    Compression {
        /// The payload to measure.
        #[arg(long)]
        input: PathBuf,
        /// Comma-separated algos, e.g. zstd-1,zstd-3,zstd-19,brotli-1,brotli-9.
        #[arg(long, default_value = "zstd-1,zstd-3,zstd-9,zstd-19,brotli-1,brotli-9")]
        algos: String,
        /// Write the report as markdown to this path.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Benchmark the external `butler` binary's *complete* patch pipeline
    /// (v0.8.0): diff, rediff --rediff-quality 9, apply and verify for
    /// both the default and the optimized patch, with times and peak RSS.
    ButlerFull {
        /// Old build (file or directory).
        #[arg(long)]
        old: PathBuf,
        /// New build (same kind as --old).
        #[arg(long)]
        new: PathBuf,
        /// Path to the butler binary (default: `butler` on PATH).
        #[arg(long, default_value = "butler")]
        butler_bin: String,
        /// Results directory.
        #[arg(long)]
        out: PathBuf,
    },
    /// The proof report (v0.8.0): every CAVS route (chunks, plan, sidecar,
    /// auto-route) and the full external butler pipeline on one pair, one
    /// table, honest win/loss verdicts. CAVS apply times/RSS measured via
    /// real subprocesses, all outputs verified byte-identical.
    FullPipeline {
        /// Old build (file or directory).
        #[arg(long)]
        old: PathBuf,
        /// New build (same kind as --old).
        #[arg(long)]
        new: PathBuf,
        /// Also run the external butler harness with this binary.
        #[arg(long)]
        butler_bin: Option<String>,
        /// Include butler rediff (optimized patch) in the butler run.
        #[arg(long, default_value_t = true)]
        include_rediff: bool,
        /// Include bsdiff/xdelta3 pairwise proxies.
        #[arg(long)]
        include_pairwise: bool,
        /// Results directory.
        #[arg(long)]
        out: PathBuf,
    },
    /// Benchmark the external `butler` binary's offline diff/apply/verify
    /// pipeline on a real old/new pair (v0.7.0). Measures the default
    /// patch only; `bench butler-full` also measures the optimized one.
    ButlerOffline {
        /// Old build (file or directory).
        #[arg(long)]
        old: PathBuf,
        /// New build (same kind as --old).
        #[arg(long)]
        new: PathBuf,
        /// Path to the butler binary (default: `butler` on PATH).
        #[arg(long, default_value = "butler")]
        butler_bin: String,
        /// Results directory.
        #[arg(long)]
        out: PathBuf,
    },
    /// Approximate the optimized pairwise patch class (bsdiff/xdelta3 +
    /// recompression) with transparent local tools (v0.7.0). Results are
    /// always labeled as a proxy.
    PairwiseProxy {
        /// Old build (file or directory).
        #[arg(long)]
        old: PathBuf,
        /// New build (same kind as --old).
        #[arg(long)]
        new: PathBuf,
        /// Comma-separated delta tools.
        #[arg(long, default_value = "bsdiff,xdelta3")]
        algos: String,
        /// Comma-separated recompressions (zstd-N, brotli-N, none).
        #[arg(long, default_value = "zstd-19,brotli-9")]
        compression: String,
        /// Results directory.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Compare every delivery route for one old→new transition (v0.7.0):
    /// full downloads, CAVS chunk/hybrid, CAVS offline plan, butler
    /// offline, pairwise proxies. Missing tools are skipped, not fatal.
    Routes {
        /// Old build (file or directory).
        #[arg(long)]
        old: PathBuf,
        /// New build (same kind as --old).
        #[arg(long)]
        new: PathBuf,
        /// Also run the external butler harness with this binary.
        #[arg(long)]
        butler_bin: Option<String>,
        /// Include bsdiff/xdelta3 optimized pairwise proxies.
        #[arg(long)]
        include_pairwise_proxy: bool,
        /// Results directory.
        #[arg(long)]
        out: PathBuf,
    },
    /// Generate a deterministic synthetic *directory* build pair
    /// (Build_v1/, Build_v2/) with modified, new, deleted and renamed
    /// files — the shapes that matter for per-file update delivery.
    GenDir {
        /// Output dataset directory.
        #[arg(long)]
        out: PathBuf,
        /// Approximate build size, e.g. 128MiB.
        #[arg(long, default_value = "128MiB")]
        size: String,
        /// PRNG seed (same seed + size => identical trees).
        #[arg(long, default_value_t = 5)]
        seed: u64,
    },
    /// Pack-pathology benchmark (v0.9.0): deterministic layouts
    /// (localized, shifted, shuffled, distributed TOC, global vs
    /// per-asset compression, new packs, directory builds, Godot PCKs)
    /// measured under the SteamPipe-style model, real .cavsplans and
    /// external tools when installed.
    SteampipeCases {
        /// Results directory.
        #[arg(long)]
        out: PathBuf,
        /// Assets (1 MiB each) per generated pack.
        #[arg(long, default_value_t = 32)]
        assets: usize,
        /// PRNG seed (same seed => identical datasets).
        #[arg(long, default_value_t = 9)]
        seed: u64,
        /// Also run the external butler harness with this binary.
        #[arg(long)]
        butler_bin: Option<String>,
        /// Include bsdiff/xdelta3 pairwise proxies when installed.
        #[arg(long)]
        include_pairwise: bool,
        /// Keep the generated datasets on disk after the run.
        #[arg(long)]
        keep_datasets: bool,
    },
    /// Estimate a SteamPipe-style fixed-chunk update for one old→new
    /// transition (v0.9.0). A public model — not Valve's implementation,
    /// and the report says so.
    SteampipeStyle {
        /// Old build (file or directory).
        old: PathBuf,
        /// New build (same kind as old).
        new: PathBuf,
        /// Fixed chunk size (default 1MiB, the documented SteamPipe size).
        #[arg(long)]
        chunk_size: Option<String>,
        /// Chunk hash (only blake3 today).
        #[arg(long, default_value = "blake3")]
        hash: String,
        /// Transfer estimate compression: none|zstd-3|zstd-19.
        #[arg(long, default_value = "zstd-3")]
        compression: String,
        /// Chunk match scope: per-file (documented model) or global.
        #[arg(long, default_value = "per-file")]
        scope: String,
        /// Exclude entries matching this glob (repeatable; merged with
        /// `.cavsignore`).
        #[arg(long)]
        ignore: Vec<String>,
        /// Machine-readable JSON on stdout.
        #[arg(long)]
        json: bool,
        /// Also write a Markdown report to this path.
        #[arg(long)]
        markdown: Option<PathBuf>,
        /// Results directory (steampipe-style.md + .json).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Compare practical patch graph policies (v1.1.0): adjacent-only,
    /// sparse dyadic ladder, base hub, hot pairs under a storage budget,
    /// the all-pairs one-hop theoretical baseline, and CAVS
    /// content-addressed routes — under an explicit user traffic model.
    PatchPolicy {
        /// Ordered version builds (files or directories).
        #[arg(long, num_args = 1..)]
        versions: Vec<PathBuf>,
        /// Directory of ordered builds (alternative to --versions).
        #[arg(long)]
        versions_dir: Option<PathBuf>,
        /// Only entries matching this pattern (`*`/`?`).
        #[arg(long, default_value = "*")]
        version_glob: String,
        /// name | semver | mtime.
        #[arg(long, default_value = "semver")]
        sort: String,
        /// Comma-separated policies to compare.
        #[arg(long, default_value = patch_policy_bench::DEFAULT_POLICIES)]
        policies: String,
        /// Comma-separated engines (cavsplan,bsdiff,xdelta3,butler-offline);
        /// missing tools are skipped with a recorded reason, never fatal.
        #[arg(long, default_value = patch_policy_bench::DEFAULT_ENGINES)]
        patch_engines: String,
        /// Built-in model (adjacent-heavy, skip-heavy, live-service-weekly,
        /// major-release, random) or custom:file.toml.
        #[arg(long, default_value = "adjacent-heavy")]
        traffic_model: String,
        /// Override the traffic model's user count.
        #[arg(long)]
        users: Option<u64>,
        /// cold-cache-with-previous-install | warm-cache.
        #[arg(long, default_value = "cold-cache-with-previous-install")]
        client_state: String,
        /// aligned (dyadic intervals, <2N patches) | dense.
        #[arg(long, default_value = "aligned")]
        ladder_mode: String,
        /// first | middle | latest-major | fixed:<id> | auto (test
        /// candidates, keep the best under the traffic model).
        #[arg(long, default_value = "auto")]
        base_policy: String,
        /// latest:K | traffic-top:K (hot-pairs policy).
        #[arg(long, default_value = "latest:3")]
        hot_pairs: String,
        /// Byte budget for hot-pair patches: 1GiB | 2x-latest-build | ….
        #[arg(long)]
        patch_storage_budget: Option<String>,
        /// Recompression measured on external patch artifacts (zstd-N).
        #[arg(long, default_value = "zstd-19")]
        compression: String,
        /// Keep patch artifacts under out/raw/<engine>/.
        #[arg(long)]
        keep_patches: bool,
        /// Results directory.
        #[arg(long)]
        out: PathBuf,
    },
    /// Generate a deterministic v01…vNN release stream for the
    /// patch-policy benchmark: drift% of blocks change per release,
    /// with an optional major content change at one release.
    GenStream {
        /// Output directory (one .bin per version).
        #[arg(long)]
        out: PathBuf,
        /// Size of each version.
        #[arg(long, default_value = "32MiB")]
        size: String,
        /// Number of releases in the stream (2–200).
        #[arg(long, default_value_t = 10)]
        versions: usize,
        /// PRNG seed (same seed + size ⇒ identical stream).
        #[arg(long, default_value_t = 5)]
        seed: u64,
        /// Percent of blocks changed per release.
        #[arg(long, default_value_t = 3)]
        drift_pct: u32,
        /// 1-based release index that rewrites 60% of blocks (a major
        /// content/layout change), e.g. 10 for v10.
        #[arg(long)]
        major_at: Option<usize>,
    },
    /// Many-version stream (v0.7.0): v1→vN with ~3% drift per release;
    /// compares CAVS store-once delivery against pairwise patch storage
    /// for adjacent updates, long jumps and reinstalls.
    VersionStream {
        /// Results directory.
        #[arg(long)]
        out: PathBuf,
        /// Size of each version.
        #[arg(long, default_value = "32MiB")]
        size: String,
        /// Number of releases in the stream.
        #[arg(long, default_value_t = 10)]
        versions: usize,
        /// PRNG seed.
        #[arg(long, default_value_t = 5)]
        seed: u64,
    },
}

#[derive(Subcommand)]
enum PatchPolicyAction {
    /// Build a patch graph (structure only, no diffs) for a set of
    /// policies over ordered builds; `bench patch-policy` measures it.
    Graph {
        /// Ordered version builds (files or directories).
        #[arg(long, num_args = 1..)]
        versions: Vec<PathBuf>,
        /// Directory of ordered builds (alternative to --versions).
        #[arg(long)]
        versions_dir: Option<PathBuf>,
        /// Only entries matching this pattern (`*`/`?`).
        #[arg(long, default_value = "*")]
        version_glob: String,
        /// name | semver | mtime.
        #[arg(long, default_value = "semver")]
        sort: String,
        /// Comma-separated policies (adjacent,ladder,base,hot-pairs,all-pairs,cavs).
        #[arg(long, default_value = "adjacent,ladder,base,all-pairs")]
        policies: String,
        /// aligned (dyadic intervals, <2N patches) | dense.
        #[arg(long, default_value = "aligned")]
        ladder_mode: String,
        /// first | middle | latest-major | fixed:<id> | auto.
        #[arg(long, default_value = "auto")]
        base_policy: String,
        /// latest:K | traffic-top:K (hot-pairs policy).
        #[arg(long, default_value = "latest:3")]
        hot_pairs: String,
        /// Output graph JSON path.
        #[arg(long)]
        out: PathBuf,
    },
    /// Replay a traffic model over a measured patch graph.
    Simulate {
        /// patch_graph.json from `cavs bench patch-policy`.
        #[arg(long)]
        graph: PathBuf,
        /// Built-in model name or custom:file.toml.
        #[arg(long, default_value = "adjacent-heavy")]
        traffic_model: String,
        /// Override the model's user count.
        #[arg(long)]
        users: Option<u64>,
        /// cold-cache-with-previous-install | warm-cache.
        #[arg(long, default_value = "cold-cache-with-previous-install")]
        client_state: String,
        /// Also write the summary as Markdown to this path.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Show the exact path one policy takes for one old→new query.
    Explain {
        /// patch_graph.json from `cavs bench patch-policy`.
        #[arg(long)]
        graph: PathBuf,
        /// Old version id (as recorded in the graph).
        #[arg(long)]
        from: String,
        /// New version id.
        #[arg(long)]
        to: String,
        /// Policy name (adjacent, ladder, base, hot-pairs, all-pairs, cavs).
        #[arg(long)]
        policy: String,
    },
}

#[derive(Subcommand)]
enum ManifestAction {
    /// Export the manifest of a .cavs as human-readable JSON (v1 format).
    Export {
        /// The .cavs file.
        input: PathBuf,
        /// Output path (stdout when omitted).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Compare manifest formats for a .cavs: wire size, parse time,
    /// bytes per logical chunk.
    Bench {
        /// The .cavs file.
        input: PathBuf,
        /// Also write the report as JSON to this path.
        #[arg(long)]
        json: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum StorageArg {
    /// One file per chunk (pre-0.4.0 layout).
    Loose,
    /// Chunks appended into immutable .cavspack files, read by range.
    Packfiles,
}

#[derive(Subcommand)]
enum StoreAction {
    /// Ingest a .cavs into the store, deduplicating its chunks.
    Add {
        /// Asset name (e.g. game_v42).
        name: String,
        /// The .cavs file to ingest.
        cavs: PathBuf,
        /// Physical chunk layout; applies when the store is created (an
        /// existing store keeps its layout).
        #[arg(long, value_enum)]
        storage: Option<StorageArg>,
    },
    /// Unpublish an asset (chunks it uniquely held become reclaimable by gc).
    Rm { name: String },
    /// Remove zero-ref chunks that have been unreferenced for --grace
    /// seconds; a packfile is deleted once no live chunk references it.
    Gc {
        #[arg(long, default_value_t = 0)]
        grace: u64,
    },
    /// Show assets, storage savings and packfile occupancy.
    Stat,
    /// Re-hash every chunk (loose or packed) and check pack integrity.
    Verify,
    /// Export a packfile store as a deterministic immutable object tree,
    /// ready to upload to S3/R2/a static host behind a CDN.
    Export {
        /// Output directory (created if missing).
        #[arg(long)]
        out: PathBuf,
        /// Also write per-asset `chunk-map.json` files (v0.6.0): everything
        /// a static client needs to plan a fetch without a smart server.
        #[arg(long)]
        static_plans: bool,
    },
    /// Migrate the ledger from monolithic index.bin to the segmented,
    /// mmapped index (Round 3B): opens stay sub-second and publishes append
    /// delta segments instead of rewriting the whole ledger. One-way;
    /// index.bin is kept as index.bin.pre-migration for manual rollback
    /// (delete index/ and rename it back).
    IndexMigrate,
    /// Report the ledger's index mode, generation, segments and deltas.
    IndexInspect,
    /// Round 3D telemetry: per-pack live/dead bytes, small-pack ratio and
    /// a comparative fragmentation score.
    Fragmentation,
    /// Merge small packs and compact packs with excessive dead bytes,
    /// copy-on-write (old packs go to quarantine, recoverable). Re-export
    /// affected assets afterwards if a static tree serves this store.
    Repack {
        /// Plan and report only; write nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Pack {
            inputs,
            output,
            raw,
            segment_time,
            mode,
            chunk_size,
            profile,
            prev,
            bootstrap,
            no_compress,
            zstd_level,
            transcode,
            sign_key,
            against_signature,
        } => {
            let opts = pack::PackOptions {
                segment_time,
                mode,
                chunk_size,
                profile,
                prev,
                bootstrap,
                compress: !no_compress,
                zstd_level,
                force_transcode: transcode,
                sign_key,
                against_signature,
            };
            if raw {
                pack::pack_raw(&inputs, &output, &opts)
            } else {
                pack::pack_video(&inputs, &output, &opts)
            }
        }
        Command::PackDir {
            input,
            output,
            profile,
            no_compress,
            zstd_level,
            sign_key,
            ignore,
        } => pack_dir::pack_dir(
            &input,
            &output,
            &pack_dir::PackDirOptions {
                profile,
                compress: !no_compress,
                zstd_level,
                sign_key,
                ignore,
            },
        ),
        Command::Signature { action } => match action {
            SignatureAction::Export {
                input,
                raw,
                block_kib,
                output,
            } => signature_cmd::export(&input, raw, block_kib.max(1) * 1024, &output),
            SignatureAction::Inspect { input, json } => signature_cmd::inspect(&input, json),
            SignatureAction::Ls { input, json } => signature_cmd::ls(&input, json),
            SignatureAction::Verify { input, against } => signature_cmd::verify(&input, &against),
        },
        Command::Preview {
            new_build,
            against,
            changes_only,
            detect_compressed_blobs,
            json,
        } => preview::preview(
            &new_build,
            &against,
            changes_only,
            detect_compressed_blobs,
            json,
        ),
        Command::DiffPlan {
            old,
            new,
            out,
            old_signature,
            analysis,
            block_kib,
            zstd_level,
            report,
        } => diff_plan::diff_plan(&diff_plan::DiffPlanArgs {
            old: old.as_deref(),
            old_signature: old_signature.as_deref(),
            new: &new,
            out: &out,
            analysis,
            block_kib,
            zstd_level,
            report: report.as_deref(),
        }),
        Command::Apply {
            old,
            plan,
            out,
            inplace,
            verify,
            delete_removed_files,
            check_old,
            resume,
            json,
        } => apply_cmd::apply(&apply_cmd::ApplyArgs {
            old: old.as_deref(),
            plan: plan.as_deref(),
            out: out.as_deref(),
            inplace,
            verify,
            delete_removed: delete_removed_files,
            check_old,
            resume: resume.as_deref(),
            json,
        }),
        Command::VerifyInstall {
            target,
            signature,
            manifest,
            allow_extra_files,
            json,
        } => verify_install::verify_install(
            &target,
            signature.as_deref(),
            manifest.as_deref(),
            allow_extra_files,
            json,
        ),
        Command::File { input, json } => inspect_cmd::file_info(&input, json),
        Command::Ls { input, json } => inspect_cmd::ls(&input, json),
        Command::OptimizePatch {
            old,
            new,
            algo,
            compression,
            explain_strategies,
            out,
        } => {
            let report = patch_v2::generate(
                &old,
                &new,
                &patch_v2::GenerateOptions { algo, compression },
                &out,
            )?;
            println!(
                "sidecar : {} ({} for {} → {}, {} ms)",
                out.display(),
                report::human_bytes(report.patch_bytes),
                report::human_bytes(report.old_total_size),
                report::human_bytes(report.new_total_size),
                report.gen_ms,
            );
            println!(
                "files   : {} copy-old ({} renames) · {} plan-ops · {} bsdiff · {} xdelta3 · {} full-data · {} deletions",
                report.files_copy_old,
                report.renames_detected,
                report.files_plan_ops,
                report.files_bsdiff,
                report.files_xdelta3,
                report.files_full_data,
                report.deleted,
            );
            if !report.skipped_tools.is_empty() {
                println!(
                    "note    : candidates not measured (tool missing): {}",
                    report.skipped_tools.join(", ")
                );
            }
            println!(
                "note    : sidecars serve exactly this old→new pair; generate them only \
                 for hot pairs (cavs patch-policy)"
            );
            if let Some(path) = explain_strategies {
                std::fs::write(&path, patch_v2::explain_markdown(&report))?;
                println!("report  : {}", path.display());
            }
            Ok(())
        }
        Command::ApplyPatch {
            old,
            patch,
            out,
            memory_budget,
            delete_removed_files,
            check_old,
        } => {
            let magic = {
                let mut f = std::fs::File::open(&patch)?;
                let mut m = [0u8; 8];
                use std::io::Read as _;
                let _ = f.read(&mut m)?;
                m
            };
            if magic == *b"CAVSPCH1" {
                optimize_patch::apply(&old, &patch, &out)
            } else {
                let budget = memory_budget
                    .as_deref()
                    .map(synth::parse_size_pub)
                    .transpose()?;
                let stats = patch_v2::apply(
                    &patch,
                    &old,
                    &out,
                    &patch_v2::ApplyV2Options {
                        delete_removed: delete_removed_files,
                        memory_budget_bytes: budget,
                        check_old,
                    },
                )?;
                println!(
                    "apply   : OK — {} ({} written, {} no-op, {} deleted, {} ms)",
                    out.display(),
                    stats.files_written,
                    stats.files_noop,
                    stats.deleted,
                    stats.elapsed_ms,
                );
                Ok(())
            }
        }
        Command::RoutePlan {
            installed,
            new,
            plan,
            patch,
            bootstrap,
            profile,
            json,
        } => route_plan::route_plan(&route_plan::RoutePlanArgs {
            installed: installed.as_deref(),
            new: &new,
            plan: plan.as_deref(),
            patch: patch.as_deref(),
            bootstrap: bootstrap.as_deref(),
            profile,
            json,
        }),
        Command::PatchPolicy {
            action,
            versions,
            distribution,
            config,
            json,
        } => match action {
            Some(PatchPolicyAction::Graph {
                versions,
                versions_dir,
                version_glob,
                sort,
                policies,
                ladder_mode,
                base_policy,
                hot_pairs,
                out,
            }) => patch_policy_bench::graph_cmd(&patch_policy_bench::GraphArgs {
                versions,
                versions_dir,
                version_glob,
                sort,
                policies,
                ladder_mode,
                base_policy,
                hot_pairs,
                out,
            }),
            Some(PatchPolicyAction::Simulate {
                graph,
                traffic_model,
                users,
                client_state,
                out,
            }) => patch_policy_bench::simulate_cmd(
                &graph,
                &traffic_model,
                users,
                &client_state,
                out.as_deref(),
            ),
            Some(PatchPolicyAction::Explain {
                graph,
                from,
                to,
                policy,
            }) => patch_policy_bench::explain_cmd(&graph, &from, &to, &policy),
            None => {
                let versions = versions.ok_or_else(|| {
                    anyhow::anyhow!(
                        "pass --versions v1,v2,… for sidecar planning, or use a subcommand \
                         (graph, simulate, explain)"
                    )
                })?;
                patch_policy::run(&versions, distribution.as_deref(), config.as_deref(), json)
            }
        },
        Command::PublishDir {
            build,
            previous,
            out_dir,
            optimize_patches,
            ignore,
            zstd_level,
            sign_key,
            preview,
        } => publish_dir::publish_dir(&publish_dir::PublishArgs {
            build: &build,
            previous: previous.as_deref(),
            out_dir: &out_dir,
            optimize_patches,
            ignore,
            zstd_level,
            sign_key: sign_key.as_deref(),
            preview_only: preview,
        }),
        Command::Sweep {
            input,
            prev,
            profiles,
            zstd_level,
            json,
        } => sweep::sweep(
            &input,
            prev.as_deref(),
            profiles.as_deref(),
            zstd_level,
            json.as_deref(),
        ),
        Command::Unpack {
            input,
            output,
            no_mp4,
        } => unpack::unpack(&input, &output, !no_mp4).map(|_| ()),
        Command::Info {
            input,
            segments,
            chunks,
        } => report::info(&input, segments, chunks),
        Command::Verify { input, pubkey } => report::verify(&input, pubkey.as_deref()),
        Command::Keygen { output } => keygen(&output),
        Command::Play { input } => unpack::play(&input),
        Command::Store { dir, action } => match action {
            StoreAction::Add {
                name,
                cavs,
                storage,
            } => store::add(&dir, &name, &cavs, storage),
            StoreAction::Rm { name } => store::remove(&dir, &name),
            StoreAction::Gc { grace } => store::gc(&dir, grace),
            StoreAction::Stat => store::stat(&dir),
            StoreAction::Verify => store::verify(&dir),
            StoreAction::Export { out, static_plans } => store::export(&dir, &out, static_plans),
            StoreAction::IndexMigrate => store::index_migrate(&dir),
            StoreAction::IndexInspect => store::index_inspect(&dir),
            StoreAction::Fragmentation => store::fragmentation(&dir),
            StoreAction::Repack { dry_run } => store::repack(&dir, dry_run),
        },
        Command::Manifest { action } => match action {
            ManifestAction::Export { input, out } => manifest_cmd::export(&input, out.as_deref()),
            ManifestAction::Bench { input, json } => manifest_cmd::bench(&input, json.as_deref()),
        },
        Command::Doctor {
            input,
            store,
            cache,
        } => doctor::doctor(input.as_deref(), store.as_deref(), cache.as_deref()),
        Command::Test { action } => match action {
            TestAction::Corrupt { input, out } => corrupt::corrupt(&input, out.as_deref()),
            TestAction::ApplyRecovery { old, new, out } => {
                test_recovery::run(&old, &new, out.as_deref())
            }
        },
        Command::Bench { action } => match action {
            BenchAction::PatchPolicy {
                versions,
                versions_dir,
                version_glob,
                sort,
                policies,
                patch_engines,
                traffic_model,
                users,
                client_state,
                ladder_mode,
                base_policy,
                hot_pairs,
                patch_storage_budget,
                compression,
                keep_patches,
                out,
            } => patch_policy_bench::bench(&patch_policy_bench::BenchArgs {
                versions,
                versions_dir,
                version_glob,
                sort,
                policies,
                patch_engines,
                traffic_model,
                users,
                client_state,
                ladder_mode,
                base_policy,
                hot_pairs,
                patch_storage_budget,
                compression,
                keep_patches,
                out,
            }),
            BenchAction::GenStream {
                out,
                size,
                versions,
                seed,
                drift_pct,
                major_at,
            } => patch_policy_bench::gen_stream(&out, &size, versions, seed, drift_pct, major_at),
            BenchAction::Gen { out, size, seed } => synth::generate(&out, &size, seed),
            BenchAction::GenDir { out, size, seed } => synth::generate_dir(&out, &size, seed),
            BenchAction::Suite { dataset, out } => synth::suite(&dataset, &out),
            BenchAction::Delta { old, new, out } => bench_delta::bench(&old, &new, out.as_deref()),
            BenchAction::Compression { input, algos, out } => {
                bench_compression::bench(&input, &algos, out.as_deref())
            }
            BenchAction::ButlerOffline {
                old,
                new,
                butler_bin,
                out,
            } => bench_butler::bench(&old, &new, &butler_bin, &out),
            BenchAction::ButlerFull {
                old,
                new,
                butler_bin,
                out,
            } => bench_butler_full::bench(&old, &new, &butler_bin, &out),
            BenchAction::FullPipeline {
                old,
                new,
                butler_bin,
                include_rediff,
                include_pairwise,
                out,
            } => bench_pipeline::bench(&bench_pipeline::PipelineArgs {
                old: &old,
                new: &new,
                butler_bin: butler_bin.as_deref(),
                include_rediff,
                include_pairwise,
                out: &out,
            }),
            BenchAction::PairwiseProxy {
                old,
                new,
                algos,
                compression,
                out,
            } => bench_pairwise::bench(&old, &new, &algos, &compression, out.as_deref()),
            BenchAction::Routes {
                old,
                new,
                butler_bin,
                include_pairwise_proxy,
                out,
            } => bench_routes::bench(&bench_routes::RoutesArgs {
                old: &old,
                new: &new,
                butler_bin: butler_bin.as_deref(),
                include_pairwise_proxy,
                out: &out,
            }),
            BenchAction::SteampipeCases {
                out,
                assets,
                seed,
                butler_bin,
                include_pairwise,
                keep_datasets,
            } => bench_steampipe_cases::bench(&bench_steampipe_cases::CasesArgs {
                out: &out,
                assets,
                seed,
                butler_bin: butler_bin.as_deref(),
                include_pairwise,
                keep_datasets,
            }),
            BenchAction::SteampipeStyle {
                old,
                new,
                chunk_size,
                hash,
                compression,
                scope,
                ignore,
                json,
                markdown,
                out,
            } => {
                if hash != "blake3" {
                    anyhow::bail!("CAVS-E-STEAMPIPE-MODEL-INVALID: only blake3 is supported");
                }
                steampipe_cmd::bench(&steampipe_cmd::BenchArgs {
                    old: &old,
                    new: &new,
                    chunk_size: chunk_size.as_deref(),
                    compression: &compression,
                    scope: &scope,
                    ignore,
                    json,
                    markdown: markdown.as_deref(),
                    out: out.as_deref(),
                })
            }
            BenchAction::VersionStream {
                out,
                size,
                versions,
                seed,
            } => bench_versions::bench(&out, &size, versions, seed),
        },
        Command::Analyze { action } => match action {
            AnalyzeAction::Steampipe {
                old,
                new,
                engine,
                ignore,
                out,
                json,
            } => steampipe_cmd::analyze_steampipe(&steampipe_cmd::AnalyzeArgs {
                old: &old,
                new: &new,
                engine: &engine,
                ignore,
                json,
                out: out.as_deref(),
            }),
            AnalyzeAction::UpdateCost {
                old,
                new,
                model,
                json,
            } => {
                if model != "steampipe-style" {
                    anyhow::bail!(
                        "CAVS-E-STEAMPIPE-MODEL-INVALID: unknown model '{model}' \
                         (only steampipe-style today)"
                    );
                }
                steampipe_cmd::bench(&steampipe_cmd::BenchArgs {
                    old: &old,
                    new: &new,
                    chunk_size: None,
                    compression: "zstd-3",
                    scope: "per-file",
                    ignore: Vec::new(),
                    json,
                    markdown: None,
                    out: None,
                })
            }
            AnalyzeAction::GodotPck {
                old,
                new,
                out,
                json,
            } => analyze_packs::analyze_godot_pck(&analyze_packs::GodotArgs {
                old: &old,
                new: &new,
                out: out.as_deref(),
                json,
            }),
        },
        Command::AnalyzePacks {
            old,
            new,
            engine,
            out,
            json,
        } => analyze_packs::analyze_packs(&analyze_packs::PacksArgs {
            old: &old,
            new: &new,
            engine: &engine,
            out: out.as_deref(),
            json,
        }),
        Command::IoEstimate {
            old,
            new,
            device_profiles,
            out,
            json,
        } => io_estimate::io_estimate(&io_estimate::IoArgs {
            old: &old,
            new: &new,
            device_profiles: device_profiles.as_deref(),
            out: out.as_deref(),
            json,
        }),
        Command::PublishPreview {
            build,
            previous,
            workspace,
            app,
            from,
            to,
            routes,
            butler_bin,
            out,
            json,
        } => {
            let all = routes == "all";
            let butler = match &butler_bin {
                Some(b) => Some(b.as_str()),
                None if all || routes.contains("butler") => Some("butler"),
                None => None,
            };
            publish_preview::publish_preview(&publish_preview::PreviewArgs {
                build: build.as_deref(),
                previous: previous.as_deref(),
                workspace: workspace.as_deref(),
                app: app.as_deref(),
                from_build: from.as_deref(),
                to_build: to.as_deref(),
                butler_bin: butler,
                include_pairwise: all || routes.contains("bsdiff") || routes.contains("xdelta"),
                out: out.as_deref(),
                json,
            })
        }
        Command::PlanUpdate {
            from,
            to,
            plan,
            patch,
            bootstrap,
            client_state,
            policy,
            json,
        } => plan_update::plan_update(&plan_update::PlanUpdateArgs {
            from: from.as_deref(),
            to: &to,
            plan_file: plan.as_deref(),
            patch_file: patch.as_deref(),
            bootstrap_file: bootstrap.as_deref(),
            client_state: &client_state,
            policy: &policy,
            json,
        }),
        Command::OptimizeLayout {
            old,
            new,
            engine,
            out,
            write_plan,
            json,
        } => optimize_layout::optimize_layout(&optimize_layout::LayoutArgs {
            old: &old,
            new: &new,
            engine: &engine,
            out: out.as_deref(),
            write_plan: write_plan.as_deref(),
            json,
        }),
        Command::Workspace { action } => match action {
            WorkspaceAction::Init { path, app } => workspace_cmd::init(&path, &app),
        },
        Command::Depot { action } => match action {
            DepotAction::Add {
                id,
                name,
                platform,
                language,
                optional,
                workspace,
                app,
            } => workspace_cmd::depot_add(
                &workspace,
                app.as_deref(),
                &id,
                name.as_deref(),
                platform.as_deref(),
                language.as_deref(),
                optional,
            ),
            DepotAction::AnalyzeSharing {
                workspace,
                app,
                build,
                out,
                json,
            } => workspace_cmd::depot_analyze_sharing(
                &workspace,
                app.as_deref(),
                build.as_deref(),
                out.as_deref(),
                json,
            ),
        },
        Command::Branch { action } => match action {
            BranchAction::Add {
                id,
                name,
                private,
                workspace,
                app,
            } => {
                workspace_cmd::branch_add(&workspace, app.as_deref(), &id, name.as_deref(), private)
            }
            BranchAction::Promote {
                workspace,
                app,
                branch,
                build,
            } => workspace_cmd::branch_promote(&workspace, app.as_deref(), &branch, &build),
            BranchAction::PromotePreview {
                workspace,
                app,
                branch,
                build,
                json,
            } => workspace_cmd::branch_promote_preview(
                &workspace,
                app.as_deref(),
                &branch,
                &build,
                json,
            ),
            BranchAction::Rollback {
                workspace,
                app,
                branch,
                to,
            } => workspace_cmd::branch_rollback(&workspace, app.as_deref(), &branch, &to),
        },
        Command::Build { action } => match action {
            BuildAction::Create {
                workspace,
                app,
                branch,
                depot,
                label,
            } => workspace_cmd::build_create(
                &workspace,
                app.as_deref(),
                branch.as_deref(),
                label.as_deref(),
                &depot,
            ),
            BuildAction::Sign { artifact, key, out } => {
                build_cmd::sign(&artifact, &key, out.as_deref())
            }
            BuildAction::Verify {
                artifact,
                pubkey,
                sig,
            } => build_cmd::verify(&artifact, &pubkey, sig.as_deref()),
            BuildAction::Encrypt { artifact, key, out } => {
                build_cmd::encrypt(&artifact, &key, &out)
            }
            BuildAction::Decrypt { artifact, key, out } => {
                build_cmd::decrypt(&artifact, &key, &out)
            }
            BuildAction::ContentKey { out } => build_cmd::generate_content_key(&out),
        },
        Command::InstallPlan {
            workspace,
            app,
            branch,
            platform,
            language,
            owned,
            from,
            to,
            out,
            json,
        } => workspace_cmd::install_plan(&workspace_cmd::InstallPlanArgs {
            workspace: &workspace,
            app: app.as_deref(),
            branch: branch.as_deref(),
            platform: platform.as_deref(),
            language: language.as_deref(),
            owned: owned
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect(),
            from_build: from.as_deref(),
            to_build: to.as_deref(),
            json,
            out: out.as_deref(),
        }),
        Command::Certify { action, full } => {
            let code = certify::dispatch(action, &full);
            std::process::exit(code);
        }
        Command::Serve {
            workspace,
            app,
            branch,
            releases,
            port,
        } => serve_cmd::serve(&serve_cmd::ServeArgs {
            workspace,
            app,
            branch,
            releases,
            port,
        }),
    }
}

fn keygen(output: &std::path::Path) -> Result<()> {
    use rand_core::OsRng;
    let key = ed25519_dalek::SigningKey::generate(&mut OsRng);
    let secret_hex: String = key.to_bytes().iter().map(|b| format!("{b:02x}")).collect();
    let public_hex: String = key
        .verifying_key()
        .to_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    std::fs::write(output, format!("{secret_hex}\n"))?;
    std::fs::write(output.with_extension("pub"), format!("{public_hex}\n"))?;
    println!("secret : {} (keep private)", output.display());
    println!("public : {}", output.with_extension("pub").display());
    println!("pubkey : {public_hex}");
    Ok(())
}
