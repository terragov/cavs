//! Offline plan execution: turn an old install into the new build.
//!
//! Safety model (same principles as the online client):
//! - every reconstructed file is verified against its plan BLAKE3 *before*
//!   it can replace anything;
//! - artifact mode writes `<out>.part` and renames only after verification;
//! - directory mode reconstructs into `<out>/.cavs-staging/`, verifies,
//!   journals its intent, then commits with per-file renames — an
//!   interrupted apply is finished (or safely restarted) by re-running;
//! - unchanged files are detected by hash and never touched (mtime and
//!   user modifications to *unmanaged* files survive);
//! - the old install is only ever read.

use crate::{OfflinePlan, PlanEntry, PlanError, PlanKind, PlanMode, PlanOp};
use cavs_hash::{ChunkHash, Hasher};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, PlanError>;

pub const STAGING_DIR: &str = ".cavs-staging";
pub const JOURNAL_FILE: &str = ".cavs-journal.json";

#[derive(Debug, Clone, Default)]
pub struct ApplyOptions {
    /// Delete old paths the plan marks as removed (managed deletions).
    pub delete_removed: bool,
    /// Re-hash the whole old source against the plan's recorded BLAKE3
    /// before applying (costs one full read; output is verified regardless).
    pub check_old: bool,
    /// Recorded in the journal so `--resume` can reload the plan.
    pub plan_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ApplyStats {
    pub files_total: u64,
    /// Files reconstructed and (re)written.
    pub files_written: u64,
    /// Files already matching the plan hash — untouched.
    pub files_noop: u64,
    pub dirs_created: u64,
    pub symlinks_created: u64,
    pub deleted: u64,
    pub bytes_written: u64,
    pub bytes_from_old: u64,
    pub bytes_from_blob: u64,
    pub elapsed_ms: u64,
}

/// Journal of a directory apply: written before the commit phase so an
/// interrupted run can be finished (or cleaned) by re-running the apply.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApplyJournal {
    pub version: u32,
    /// BLAKE3 (hex) of the plan's encoded bytes — a journal only resumes
    /// the exact apply that wrote it.
    pub plan_blake3: String,
    /// Where the plan file lived when the apply started; `--resume` reloads
    /// it from here (best-effort — re-running the original command works too).
    pub plan_path: Option<PathBuf>,
    pub old_root: PathBuf,
    pub out_root: PathBuf,
    /// staging | verified | committing | committed | failed
    pub state: String,
    pub files_staged: Vec<String>,
    pub files_moved: Vec<String>,
}

impl ApplyJournal {
    pub fn path(out_root: &Path) -> PathBuf {
        out_root.join(JOURNAL_FILE)
    }

    pub fn load(out_root: &Path) -> Result<Option<Self>> {
        let path = Self::path(out_root);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&path)?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| PlanError::Journal(format!("corrupt journal {}: {e}", path.display())))
    }

    fn save(&self, out_root: &Path) -> Result<()> {
        let path = Self::path(out_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).unwrap())?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn clear(out_root: &Path) {
        let _ = std::fs::remove_file(Self::path(out_root));
    }
}

/// Hex BLAKE3 of a plan's canonical encoding — the journal's identity key.
/// Level 1 keeps re-encoding cheap; the hash is over the *decompressed*
/// logical content either way because encoding is deterministic per level.
pub fn plan_identity(plan: &OfflinePlan) -> String {
    cavs_hash::to_hex(&cavs_hash::hash_chunk(&plan.encode(1)))
}

// ---------------------------------------------------------------------------
// Artifact apply
// ---------------------------------------------------------------------------

/// Apply an artifact plan: `<out>.part` → verify → atomic rename.
pub fn apply_artifact(plan: &OfflinePlan, old: &Path, out: &Path) -> Result<ApplyStats> {
    let started = std::time::Instant::now();
    if plan.mode != PlanMode::Artifact {
        return Err(PlanError::Invalid(
            "artifact apply needs an artifact plan (use directory apply)".into(),
        ));
    }
    ensure_portable(plan)?;
    let entry = plan
        .new_entries
        .iter()
        .find(|e| e.kind == cavs_signature::EntryKind::File)
        .ok_or_else(|| PlanError::Invalid("plan has no output file".into()))?;

    let mut old_file = std::fs::File::open(old)?;
    plan.check_old_size(&mut old_file)?;

    let mut stats = ApplyStats {
        files_total: 1,
        ..Default::default()
    };
    let part = out.with_extension(format!(
        "{}part",
        out.extension()
            .map(|e| format!("{}.", e.to_string_lossy()))
            .unwrap_or_default()
    ));
    {
        let file = std::fs::File::create(&part)?;
        let mut writer = std::io::BufWriter::new(file);
        let mut hasher = Hasher::new();
        let mut sources = SingleOldSource {
            file: &mut old_file,
        };
        write_entry(plan, entry, &mut sources, |bytes, from_old| {
            hasher.update(bytes);
            stats.bytes_written += bytes.len() as u64;
            if from_old {
                stats.bytes_from_old += bytes.len() as u64;
            } else {
                stats.bytes_from_blob += bytes.len() as u64;
            }
            writer.write_all(bytes).map_err(PlanError::Io)
        })?;
        writer.flush()?;
        let got = hasher.finalize();
        if Some(got) != entry.blake3 {
            drop(writer);
            let _ = std::fs::remove_file(&part);
            return Err(PlanError::ApplyHashMismatch(entry.path.clone()));
        }
    }
    std::fs::rename(&part, out)?;
    stats.files_written = 1;
    stats.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(stats)
}

impl OfflinePlan {
    /// Cheap sanity check of the old source (size only; content is
    /// guaranteed by the output hash regardless).
    fn check_old_size(&self, old: &mut std::fs::File) -> Result<()> {
        let len = old.metadata()?.len();
        if self.mode == PlanMode::Artifact && len != self.old_size {
            return Err(PlanError::Invalid(format!(
                "old artifact is {len} bytes; the plan was diffed against {} bytes \
                 (wrong or modified old version)",
                self.old_size
            )));
        }
        Ok(())
    }
}

/// Verify the old source matches the plan's recorded full-content hash.
pub fn verify_old_source(plan: &OfflinePlan, old: &Path) -> Result<()> {
    let Some(expected) = plan.old_blake3 else {
        return Ok(());
    };
    let got = match plan.mode {
        PlanMode::Artifact => hash_file(old)?,
        PlanMode::Directory => {
            // Hash every old entry's bytes in entry order (the signature's
            // content-hash convention).
            let mut hasher = Hasher::new();
            let mut buf = vec![0u8; 1 << 20];
            for e in &plan.old_entries {
                let mut f = std::fs::File::open(old.join(&e.path))?;
                loop {
                    let n = f.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    hasher.update(&buf[..n]);
                }
            }
            hasher.finalize()
        }
    };
    if got != expected {
        return Err(PlanError::Invalid(
            "old source does not match the plan's recorded content hash".into(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Directory apply (staging + journal)
// ---------------------------------------------------------------------------

/// Apply a directory plan into `out_root` (which may equal `old_root` for
/// an in-place update). Staged, verified, journaled, committed.
pub fn apply_dir(
    plan: &OfflinePlan,
    old_root: &Path,
    out_root: &Path,
    opts: &ApplyOptions,
) -> Result<ApplyStats> {
    let started = std::time::Instant::now();
    if plan.mode != PlanMode::Directory {
        return Err(PlanError::Invalid(
            "directory apply needs a directory plan (use artifact apply)".into(),
        ));
    }
    ensure_portable(plan)?;
    if opts.check_old {
        verify_old_source(plan, old_root)?;
    }

    let identity = plan_identity(plan);
    if let Some(journal) = ApplyJournal::load(out_root)? {
        if journal.plan_blake3 != identity {
            return Err(PlanError::Journal(format!(
                "{} belongs to a different apply (plan {}…); \
                 finish it by re-running that apply, or delete the journal and staging dir",
                ApplyJournal::path(out_root).display(),
                &journal.plan_blake3[..12],
            )));
        }
        // Same plan: fall through — staging re-verification plus the
        // idempotent commit below finish whatever was interrupted.
    }

    let staging = out_root.join(STAGING_DIR);
    std::fs::create_dir_all(&staging)?;
    std::fs::create_dir_all(out_root)?;

    // Journal from the first moment staging touches disk: an interrupted or
    // failed run always leaves a machine-readable record of how far it got.
    let mut journal = ApplyJournal {
        version: 1,
        plan_blake3: identity.clone(),
        plan_path: opts.plan_path.clone(),
        old_root: old_root.to_path_buf(),
        out_root: out_root.to_path_buf(),
        state: "staging".into(),
        files_staged: Vec::new(),
        files_moved: Vec::new(),
    };
    journal.save(out_root)?;

    let mut stats = ApplyStats::default();
    let old_paths: HashMap<u32, PathBuf> = plan
        .old_entries
        .iter()
        .map(|e| (e.entry_id, old_root.join(&e.path)))
        .collect();

    // ---- Stage: reconstruct changed files, verify each one --------------
    let mut staged: Vec<(&PlanEntry, PathBuf)> = Vec::new();
    let mut old_files = OldFileCache {
        paths: &old_paths,
        open: HashMap::new(),
    };
    for entry in &plan.new_entries {
        if entry.kind != cavs_signature::EntryKind::File {
            continue;
        }
        stats.files_total += 1;
        let final_path = out_root.join(&entry.path);
        let expected = entry.blake3.expect("validated: files carry a hash");

        // No-op level: the installed file already is the new file.
        if file_matches(&final_path, entry.size, &expected) {
            stats.files_noop += 1;
            continue;
        }
        let staged_path = staging.join(format!("e{}", entry.entry_id));
        // Resume: a previously staged file that still verifies is kept.
        if file_matches(&staged_path, entry.size, &expected) {
            staged.push((entry, staged_path));
            continue;
        }

        let file = std::fs::File::create(&staged_path)?;
        let mut writer = std::io::BufWriter::new(file);
        let mut hasher = Hasher::new();
        write_entry(plan, entry, &mut old_files, |bytes, from_old| {
            hasher.update(bytes);
            stats.bytes_written += bytes.len() as u64;
            if from_old {
                stats.bytes_from_old += bytes.len() as u64;
            } else {
                stats.bytes_from_blob += bytes.len() as u64;
            }
            writer.write_all(bytes).map_err(PlanError::Io)
        })?;
        writer.flush()?;
        drop(writer);
        if hash_file(&staged_path)? != expected {
            // Abort before anything is committed; the old install and any
            // already-staged files stay intact for a corrected retry. The
            // journal records the failure so tooling can see what happened.
            journal.state = "failed".into();
            let _ = journal.save(out_root);
            return Err(PlanError::ApplyHashMismatch(entry.path.clone()));
        }
        staged.push((entry, staged_path));
    }

    // ---- Journal intent, then commit -------------------------------------
    journal.state = "verified".into();
    journal.files_staged = staged.iter().map(|(e, _)| e.path.clone()).collect();
    journal.save(out_root)?;

    // Directories first (both explicit entries and file parents).
    for entry in &plan.new_entries {
        if entry.kind == cavs_signature::EntryKind::Directory {
            let dir = out_root.join(&entry.path);
            if !dir.is_dir() {
                std::fs::create_dir_all(&dir)?;
                stats.dirs_created += 1;
            }
        }
    }
    journal.state = "committing".into();
    journal.save(out_root)?;

    for (entry, staged_path) in &staged {
        let final_path = out_root.join(&entry.path);
        if let Some(parent) = final_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(staged_path, &final_path)?;
        set_executable(&final_path, entry.executable)?;
        journal.files_moved.push(entry.path.clone());
        stats.files_written += 1;
    }
    // Executable-bit-only changes on no-op files still apply (cheap).
    for entry in &plan.new_entries {
        if entry.kind == cavs_signature::EntryKind::File {
            set_executable(&out_root.join(&entry.path), entry.executable)?;
        }
    }

    for entry in &plan.new_entries {
        if entry.kind == cavs_signature::EntryKind::Symlink {
            let link = out_root.join(&entry.path);
            let target = entry.symlink_target.as_deref().unwrap_or("");
            match create_symlink(target, &link) {
                Ok(()) => stats.symlinks_created += 1,
                Err(PlanError::UnsupportedSymlink(p)) => {
                    eprintln!(
                        "[apply] {}",
                        cavs_proto::errors::ErrorCode::UnsupportedSymlink
                            .msg(format!("skipping {p}"))
                    );
                }
                Err(e) => return Err(e),
            }
        }
    }

    if opts.delete_removed {
        for p in &plan.deleted {
            let path = out_root.join(p);
            if path.is_dir() {
                if std::fs::remove_dir(&path).is_ok() {
                    stats.deleted += 1;
                }
            } else if path.exists() || path.symlink_metadata().is_ok() {
                std::fs::remove_file(&path)?;
                stats.deleted += 1;
            }
        }
    }

    journal.state = "committed".into();
    journal.save(out_root)?;
    let _ = std::fs::remove_dir_all(&staging);
    ApplyJournal::clear(out_root);

    stats.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(stats)
}

fn ensure_portable(plan: &OfflinePlan) -> Result<()> {
    if plan.kind != PlanKind::Portable && plan.summary().inline_bytes > 0 {
        return Err(PlanError::NotPortable);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared op execution
// ---------------------------------------------------------------------------

trait OldSource {
    fn read_range(&mut self, old_entry_id: u32, offset: u64, buf: &mut [u8]) -> Result<()>;
}

struct SingleOldSource<'a> {
    file: &'a mut std::fs::File,
}

impl OldSource for SingleOldSource<'_> {
    fn read_range(&mut self, _id: u32, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(buf)?;
        Ok(())
    }
}

struct OldFileCache<'a> {
    paths: &'a HashMap<u32, PathBuf>,
    open: HashMap<u32, std::fs::File>,
}

impl OldSource for OldFileCache<'_> {
    fn read_range(&mut self, id: u32, offset: u64, buf: &mut [u8]) -> Result<()> {
        if !self.open.contains_key(&id) {
            let path = self.paths.get(&id).ok_or_else(|| {
                PlanError::Invalid(format!("op references unknown old entry {id}"))
            })?;
            self.open.insert(id, std::fs::File::open(path)?);
        }
        let file = self.open.get_mut(&id).unwrap();
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(buf)?;
        Ok(())
    }
}

/// Stream one entry's ops through `sink(bytes, from_old)`. Reads are
/// bounded (8 MiB) so peak RAM stays flat regardless of build size.
fn write_entry(
    plan: &OfflinePlan,
    entry: &PlanEntry,
    old: &mut impl OldSource,
    mut sink: impl FnMut(&[u8], bool) -> Result<()>,
) -> Result<()> {
    const READ_CHUNK: u64 = 8 * 1024 * 1024;
    let mut buf = Vec::new();
    for op in &plan.ops {
        match op {
            PlanOp::CopyOld {
                old_entry_id,
                old_offset,
                new_entry_id,
                len,
                ..
            } if *new_entry_id == entry.entry_id => {
                let mut done = 0u64;
                while done < *len {
                    let n = (*len - done).min(READ_CHUNK) as usize;
                    buf.resize(n, 0);
                    old.read_range(*old_entry_id, *old_offset + done, &mut buf)?;
                    sink(&buf, true)?;
                    done += n as u64;
                }
            }
            PlanOp::Inline {
                new_entry_id,
                len,
                blob_offset,
                ..
            } if *new_entry_id == entry.entry_id => {
                let start = *blob_offset as usize;
                let end = start + *len as usize;
                sink(&plan.blob[start..end], false)?;
            }
            _ => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn hash_file(path: &Path) -> Result<ChunkHash> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}

/// True when `path` exists, has `size` bytes and hashes to `expected`.
/// Never a false positive; any error reads as "does not match".
pub fn file_matches(path: &Path, size: u64, expected: &ChunkHash) -> bool {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() && m.len() == size => {}
        _ => return false,
    }
    hash_file(path).map(|h| h == *expected).unwrap_or(false)
}

#[cfg(unix)]
fn set_executable(path: &Path, executable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path)?;
    let mode = meta.permissions().mode();
    let want = if executable {
        mode | 0o755
    } else {
        mode & !0o111
    };
    if want != mode {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(want))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path, _executable: bool) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &str, link: &Path) -> Result<()> {
    if link.symlink_metadata().is_ok() {
        std::fs::remove_file(link)?;
    }
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

#[cfg(not(unix))]
fn create_symlink(_target: &str, link: &Path) -> Result<()> {
    Err(PlanError::UnsupportedSymlink(link.display().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{build, BuildOptions};
    use cavs_signature::{CavsSignature, DEFAULT_BLOCK_SIZE};

    fn pseudo_random(len: usize, seed: u32) -> Vec<u8> {
        let mut out = vec![0u8; len];
        let mut state = seed;
        for b in out.iter_mut() {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            *b = (state >> 24) as u8;
        }
        out
    }

    #[test]
    fn artifact_apply_is_byte_identical_and_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let old = pseudo_random(900_000, 10);
        let mut new = old.clone();
        new[100_000..130_000].copy_from_slice(&pseudo_random(30_000, 11));
        new.extend_from_slice(&pseudo_random(50_000, 12)); // grow the file
        let old_p = dir.path().join("game_v1.pck");
        let new_p = dir.path().join("game_v2.pck");
        std::fs::write(&old_p, &old).unwrap();
        std::fs::write(&new_p, &new).unwrap();

        let sig = CavsSignature::sign_file(&old_p, DEFAULT_BLOCK_SIZE, "game_v1.pck").unwrap();
        let plan = build(&sig, &new_p, &BuildOptions::default()).unwrap();

        let out = dir.path().join("rebuilt.pck");
        let stats = apply_artifact(&plan, &old_p, &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), new);
        assert!(stats.bytes_from_old > 700_000);
        assert!(!out.with_extension("pck.part").exists());
    }

    #[test]
    fn wrong_old_artifact_fails_without_committing() {
        let dir = tempfile::tempdir().unwrap();
        let old = pseudo_random(400_000, 13);
        let new = {
            let mut n = old.clone();
            n[0..100].copy_from_slice(&pseudo_random(100, 14));
            n
        };
        let old_p = dir.path().join("v1.bin");
        let new_p = dir.path().join("v2.bin");
        std::fs::write(&old_p, &old).unwrap();
        std::fs::write(&new_p, &new).unwrap();
        let sig = CavsSignature::sign_file(&old_p, DEFAULT_BLOCK_SIZE, "v1.bin").unwrap();
        let plan = build(&sig, &new_p, &BuildOptions::default()).unwrap();

        // Same size, different content: ops read wrong bytes → hash check
        // must catch it and leave no output.
        std::fs::write(&old_p, pseudo_random(400_000, 99)).unwrap();
        let out = dir.path().join("out.bin");
        let err = apply_artifact(&plan, &old_p, &out).unwrap_err();
        assert!(matches!(err, PlanError::ApplyHashMismatch(_)), "{err}");
        assert!(!out.exists());
        assert!(!out.with_extension("bin.part").exists());
    }

    fn write_tree(root: &Path, files: &[(&str, Vec<u8>)]) {
        for (rel, bytes) in files {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, bytes).unwrap();
        }
    }

    #[test]
    fn dir_apply_stages_commits_and_preserves() {
        let dir = tempfile::tempdir().unwrap();
        let old_root = dir.path().join("v1");
        let new_root = dir.path().join("v2");
        let same = pseudo_random(150_000, 20);
        let old_data = pseudo_random(200_000, 21);
        let mut new_data = old_data.clone();
        new_data[50_000..51_000].copy_from_slice(&pseudo_random(1000, 22));
        write_tree(
            &old_root,
            &[
                ("audio/music.bank", same.clone()),
                ("data/catalog.bin", old_data),
                ("old/removed.txt", b"gone".to_vec()),
            ],
        );
        write_tree(
            &new_root,
            &[
                ("audio/music.bank", same.clone()),
                ("data/catalog.bin", new_data.clone()),
                ("levels/level9.dat", pseudo_random(80_000, 23)),
            ],
        );

        let sig = CavsSignature::sign_dir(&old_root, DEFAULT_BLOCK_SIZE, "v1").unwrap();
        let plan = build(&sig, &new_root, &BuildOptions::default()).unwrap();
        assert!(plan.deleted.iter().any(|p| p == "old/removed.txt"));

        // In-place update of a copy of v1, with an extra user file present.
        let install = dir.path().join("install");
        write_tree(
            &install,
            &[
                ("audio/music.bank", same.clone()),
                (
                    "data/catalog.bin",
                    std::fs::read(old_root.join("data/catalog.bin")).unwrap(),
                ),
                ("old/removed.txt", b"gone".to_vec()),
                ("mods/user_mod.pck", b"my mod".to_vec()),
            ],
        );
        let noop_mtime = std::fs::metadata(install.join("audio/music.bank"))
            .unwrap()
            .modified()
            .unwrap();

        let stats = apply_dir(
            &plan,
            &install,
            &install,
            &ApplyOptions {
                delete_removed: true,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            std::fs::read(install.join("data/catalog.bin")).unwrap(),
            new_data
        );
        assert!(install.join("levels/level9.dat").is_file());
        assert!(!install.join("old/removed.txt").exists(), "managed delete");
        assert_eq!(
            std::fs::read(install.join("mods/user_mod.pck")).unwrap(),
            b"my mod",
            "extra files are preserved"
        );
        assert_eq!(stats.files_noop, 1, "unchanged file is a no-op");
        assert_eq!(stats.files_written, 2);
        assert_eq!(stats.deleted, 1);
        assert_eq!(
            std::fs::metadata(install.join("audio/music.bank"))
                .unwrap()
                .modified()
                .unwrap(),
            noop_mtime,
            "no-op file keeps its mtime"
        );
        assert!(!install.join(STAGING_DIR).exists());
        assert!(!ApplyJournal::path(&install).exists());

        // Re-apply: everything is a no-op now.
        let stats2 = apply_dir(&plan, &install, &install, &ApplyOptions::default()).unwrap();
        assert_eq!(stats2.files_written, 0);
        assert_eq!(stats2.files_noop, 3);
        assert_eq!(stats2.bytes_written, 0);
    }

    #[test]
    fn foreign_journal_blocks_the_apply() {
        let dir = tempfile::tempdir().unwrap();
        let old_root = dir.path().join("v1");
        let new_root = dir.path().join("v2");
        write_tree(&old_root, &[("a.bin", pseudo_random(70_000, 30))]);
        write_tree(&new_root, &[("a.bin", pseudo_random(70_000, 31))]);
        let sig = CavsSignature::sign_dir(&old_root, DEFAULT_BLOCK_SIZE, "v1").unwrap();
        let plan = build(&sig, &new_root, &BuildOptions::default()).unwrap();

        let install = dir.path().join("install");
        write_tree(&install, &[("a.bin", pseudo_random(70_000, 30))]);
        let foreign = ApplyJournal {
            version: 1,
            plan_blake3: "deadbeef".repeat(8),
            plan_path: None,
            old_root: install.clone(),
            out_root: install.clone(),
            state: "committing".into(),
            files_staged: vec![],
            files_moved: vec![],
        };
        foreign.save(&install).unwrap();
        let err = apply_dir(&plan, &install, &install, &ApplyOptions::default()).unwrap_err();
        assert!(matches!(err, PlanError::Journal(_)), "{err}");

        // Clearing the foreign journal unblocks it.
        ApplyJournal::clear(&install);
        apply_dir(&plan, &install, &install, &ApplyOptions::default()).unwrap();
        assert_eq!(
            std::fs::read(install.join("a.bin")).unwrap(),
            std::fs::read(new_root.join("a.bin")).unwrap()
        );
    }

    #[test]
    fn interrupted_staging_resumes_by_rerunning() {
        let dir = tempfile::tempdir().unwrap();
        let old_root = dir.path().join("v1");
        let new_root = dir.path().join("v2");
        let old_data = pseudo_random(300_000, 40);
        let mut new_data = old_data.clone();
        new_data[10_000..12_000].copy_from_slice(&pseudo_random(2000, 41));
        write_tree(&old_root, &[("big.bin", old_data.clone())]);
        write_tree(&new_root, &[("big.bin", new_data.clone())]);
        let sig = CavsSignature::sign_dir(&old_root, DEFAULT_BLOCK_SIZE, "v1").unwrap();
        let plan = build(&sig, &new_root, &BuildOptions::default()).unwrap();

        let install = dir.path().join("install");
        write_tree(&install, &[("big.bin", old_data)]);
        // Simulate an interrupted run: staging dir with a half-written file.
        let staging = install.join(STAGING_DIR);
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("e0"), b"partial garbage").unwrap();

        let stats = apply_dir(&plan, &install, &install, &ApplyOptions::default()).unwrap();
        assert_eq!(stats.files_written, 1);
        assert_eq!(std::fs::read(install.join("big.bin")).unwrap(), new_data);
        assert!(!staging.exists());
    }

    #[cfg(unix)]
    #[test]
    fn exec_bits_and_symlinks_travel() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let old_root = dir.path().join("v1");
        let new_root = dir.path().join("v2");
        write_tree(&old_root, &[("payload", pseudo_random(50_000, 50))]);
        write_tree(&new_root, &[("payload", pseudo_random(50_000, 51))]);
        std::fs::set_permissions(
            new_root.join("payload"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink("payload", new_root.join("play")).unwrap();

        let sig = CavsSignature::sign_dir(&old_root, DEFAULT_BLOCK_SIZE, "v1").unwrap();
        let plan = build(&sig, &new_root, &BuildOptions::default()).unwrap();

        let install = dir.path().join("install");
        write_tree(&install, &[("payload", pseudo_random(50_000, 50))]);
        apply_dir(&plan, &install, &install, &ApplyOptions::default()).unwrap();

        let mode = std::fs::metadata(install.join("payload"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "exec bit applied");
        assert_eq!(
            std::fs::read_link(install.join("play")).unwrap(),
            PathBuf::from("payload")
        );
    }
}
