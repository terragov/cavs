//! Where objects live on disk.
//!
//! An object store is a directory. Each object is one file named by its id,
//! carrying a short header that records its class, so the store describes
//! itself: presence is a `stat`, identity is recomputable from the bytes, and
//! there is no separate index that could disagree with what is actually there.
//!
//! Writes are atomic and idempotent. An object is written to a temporary file,
//! fsynced and renamed into place; storing the same object twice leaves one
//! file and costs one rename. A reader never sees a half-written object, and a
//! crash mid-write leaves a stray temporary rather than a corrupt one.

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::envelope::{DecodeLimits, ObjectEnvelope};
use crate::error::{ObjectError, Result};
use crate::id::{ObjectId, ObjectKind};
use crate::walk::{GraphSource, ObjectNode};

/// Magic at the head of every loose object file.
pub const LOOSE_MAGIC: &[u8; 8] = b"CAVSOBJ\x01";
/// Layout version of the object directory.
pub const STORE_FORMAT_V1: u16 = 1;
/// Bytes before the canonical object bytes in a loose file.
const LOOSE_HEADER_LEN: usize = 8 + 2 + 4;

/// An object, as read back out of a store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredObject {
    pub id: ObjectId,
    pub kind: ObjectKind,
    /// The canonical bytes the id was computed over.
    pub bytes: Vec<u8>,
}

impl StoredObject {
    /// Decode this object's envelope. Chunks decode to a body and no
    /// dependencies; structural objects are parsed under `limits`.
    pub fn envelope(&self, limits: DecodeLimits) -> Result<ObjectEnvelope> {
        ObjectEnvelope::decode(self.kind, &self.bytes, limits)
    }

    /// What this object depends on, without keeping the body around.
    pub fn dependencies(&self, limits: DecodeLimits) -> Result<Vec<ObjectId>> {
        Ok(self.envelope(limits)?.dependencies)
    }
}

/// What a successful verification found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyResult {
    pub id: ObjectId,
    pub kind: ObjectKind,
    pub stored_len: usize,
    pub dependencies: usize,
}

/// Outcome of sweeping a whole store.
#[derive(Debug, Clone, Default)]
pub struct StoreVerifyReport {
    pub checked: u64,
    pub bytes: u64,
    /// Objects that failed, with the reason, so one bad file does not hide
    /// the rest.
    pub failures: Vec<(ObjectId, String)>,
}

impl StoreVerifyReport {
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Reading and writing content-addressed objects.
pub trait ObjectStore {
    /// Store `bytes` as an object of class `kind` and return its id. Storing
    /// the same object twice is a no-op that returns the same id.
    fn put_object(&self, kind: ObjectKind, bytes: &[u8]) -> Result<ObjectId>;

    fn has_object(&self, id: &ObjectId) -> Result<bool>;

    /// Read an object. The bytes are checked against the id before they are
    /// returned, so a corrupt object is an error rather than a value.
    fn get_object(&self, id: &ObjectId) -> Result<StoredObject>;

    /// Re-read an object and confirm its bytes still hash to its id.
    fn verify_object(&self, id: &ObjectId) -> Result<VerifyResult>;
}

/// When an object's bytes are forced to the platter.
///
/// An object is named by its own hash, so a torn write is not silent
/// corruption: the next read recomputes the hash, the object fails, and it is
/// refetched or rewritten. That changes what durability has to buy. It does
/// not have to make every object survive a power cut — it has to make sure
/// that whatever *points* at an object is never published before the object
/// itself is safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Durability {
    /// fsync on [`FsObjectStore::flush`], once for the whole batch. The
    /// caller writes its objects, flushes, and only then publishes the
    /// reference that names them.
    #[default]
    OnFlush,
    /// fsync every object as it is written. Correct and slow: a transaction
    /// touching a handful of tree pages pays a platter round trip per page,
    /// which measures in tens of milliseconds where the work itself is
    /// microseconds.
    PerObject,
    /// Never fsync. For a store that can be rebuilt from elsewhere — a cache,
    /// a scratch import, a test.
    Never,
}

/// A directory of objects.
pub struct FsObjectStore {
    root: PathBuf,
    limits: DecodeLimits,
    durability: Durability,
    /// Objects written since the last flush, waiting to be made durable.
    pending: Mutex<Vec<PathBuf>>,
}

impl FsObjectStore {
    /// Open, creating the layout if it is not there yet.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("loose")).map_err(io_err("create object directory"))?;
        fs::create_dir_all(root.join("tmp")).map_err(io_err("create object temp directory"))?;
        let info = root.join("info");
        if !info.exists() {
            let mut f = fs::File::create(&info).map_err(io_err("write store info"))?;
            writeln!(f, "cavs-object-store {STORE_FORMAT_V1}")
                .map_err(io_err("write store info"))?;
        }
        Ok(FsObjectStore {
            root,
            limits: DecodeLimits::DEFAULT,
            durability: Durability::default(),
            pending: Mutex::new(Vec::new()),
        })
    }

    /// Use different decode limits, e.g. to admit payload-sized objects.
    pub fn with_limits(mut self, limits: DecodeLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn with_durability(mut self, durability: Durability) -> Self {
        self.durability = durability;
        self
    }

    pub fn durability(&self) -> Durability {
        self.durability
    }

    /// Make every object written since the last flush durable.
    ///
    /// Call this before publishing anything that references them. Returns how
    /// many objects were forced.
    pub fn flush(&self) -> Result<usize> {
        let pending = {
            let mut guard = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *guard)
        };
        if matches!(self.durability, Durability::Never) {
            return Ok(0);
        }
        let mut directories: BTreeSet<PathBuf> = BTreeSet::new();
        let mut forced = 0usize;
        for path in &pending {
            match fs::File::open(path) {
                Ok(file) => {
                    file.sync_all().map_err(io_err("sync object"))?;
                    forced += 1;
                }
                // Gone since it was written — garbage collected, or another
                // process pruned it. Nothing to force.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(io_err("open object for sync")(e)),
            }
            if let Some(parent) = path.parent() {
                directories.insert(parent.to_path_buf());
            }
        }
        // The names have to be durable too, or a surviving object could still
        // be unreachable after a crash.
        for directory in directories {
            if let Ok(handle) = fs::File::open(&directory) {
                let _ = handle.sync_all();
            }
        }
        Ok(forced)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn limits(&self) -> DecodeLimits {
        self.limits
    }

    /// Two-level fan-out on the first byte, so a large store does not put a
    /// million entries in one directory.
    fn loose_path(&self, id: &ObjectId) -> PathBuf {
        let hex = id.to_hex();
        self.root.join("loose").join(&hex[..2]).join(&hex[2..])
    }

    /// Store an envelope, returning its id.
    pub fn put_envelope(&self, envelope: &ObjectEnvelope) -> Result<ObjectId> {
        self.put_object(envelope.kind, &envelope.canonical_bytes())
    }

    /// Every object in the store, in id order.
    pub fn list_objects(&self) -> Result<Vec<ObjectId>> {
        let mut out = BTreeSet::new();
        let loose = self.root.join("loose");
        let Ok(shards) = fs::read_dir(&loose) else {
            return Ok(Vec::new());
        };
        for shard in shards {
            let shard = shard.map_err(io_err("scan object shard"))?;
            if !shard
                .file_type()
                .map_err(io_err("scan object shard"))?
                .is_dir()
            {
                continue;
            }
            let prefix = shard.file_name().to_string_lossy().to_string();
            for entry in fs::read_dir(shard.path()).map_err(io_err("scan object shard"))? {
                let entry = entry.map_err(io_err("scan object shard"))?;
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some(id) = ObjectId::parse_hex(&format!("{prefix}{name}")) {
                    out.insert(id);
                }
            }
        }
        Ok(out.into_iter().collect())
    }

    pub fn object_count(&self) -> Result<u64> {
        Ok(self.list_objects()?.len() as u64)
    }

    /// Verify every object in the store, collecting failures rather than
    /// stopping at the first one.
    pub fn verify_all(&self) -> Result<StoreVerifyReport> {
        let mut report = StoreVerifyReport::default();
        for id in self.list_objects()? {
            match self.verify_object(&id) {
                Ok(ok) => {
                    report.checked += 1;
                    report.bytes += ok.stored_len as u64;
                }
                Err(e) => report.failures.push((id, e.to_string())),
            }
        }
        Ok(report)
    }

    /// When an object was written, or `None` when the platform does not say.
    ///
    /// Used by collection to keep anything young: an object written a moment
    /// ago may belong to a transaction that has not published its reference
    /// yet, and to a mark-and-sweep that is indistinguishable from garbage.
    pub fn object_written_at(&self, id: &ObjectId) -> Result<Option<std::time::SystemTime>> {
        match fs::metadata(self.loose_path(id)) {
            Ok(metadata) => Ok(metadata.modified().ok()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_err("stat object")(e)),
        }
    }

    /// Delete an object. Returns whether it was there.
    pub fn remove_object(&self, id: &ObjectId) -> Result<bool> {
        let path = self.loose_path(id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_err("remove object")(e)),
        }
    }

    /// Read the raw file for an object, header included.
    fn read_loose(&self, id: &ObjectId) -> Result<Option<(ObjectKind, Vec<u8>)>> {
        let path = self.loose_path(id);
        let raw = match fs::read(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err("read object")(e)),
        };
        if raw.len() < LOOSE_HEADER_LEN {
            return Err(ObjectError::Truncated("loose object header"));
        }
        if &raw[..8] != LOOSE_MAGIC {
            return Err(ObjectError::Corrupt(format!(
                "object {id} has a bad file header"
            )));
        }
        let version = u16::from_le_bytes([raw[8], raw[9]]);
        if version != STORE_FORMAT_V1 {
            return Err(ObjectError::UnsupportedFormat(version));
        }
        let tag = u32::from_le_bytes([raw[10], raw[11], raw[12], raw[13]]);
        let kind = ObjectKind::from_tag(tag).ok_or(ObjectError::UnknownKind(tag))?;
        Ok(Some((kind, raw[LOOSE_HEADER_LEN..].to_vec())))
    }
}

impl ObjectStore for FsObjectStore {
    fn put_object(&self, kind: ObjectKind, bytes: &[u8]) -> Result<ObjectId> {
        let max = self.limits.max_len_for(kind);
        if bytes.len() > max {
            return Err(ObjectError::TooLarge {
                what: "object",
                len: bytes.len(),
                max,
            });
        }
        let id = ObjectId::compute(kind, bytes);
        let path = self.loose_path(&id);
        // Idempotent: the same object is the same file, so a second write has
        // nothing to do.
        if path.exists() {
            return Ok(id);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(io_err("create object shard"))?;
        }

        let tmp = self
            .root
            .join("tmp")
            .join(format!("{}.{}", id.to_hex(), std::process::id()));
        {
            let mut f = fs::File::create(&tmp).map_err(io_err("create temporary object"))?;
            f.write_all(LOOSE_MAGIC).map_err(io_err("write object"))?;
            f.write_all(&STORE_FORMAT_V1.to_le_bytes())
                .map_err(io_err("write object"))?;
            f.write_all(&kind.tag().to_le_bytes())
                .map_err(io_err("write object"))?;
            f.write_all(bytes).map_err(io_err("write object"))?;
            if self.durability == Durability::PerObject {
                f.sync_all().map_err(io_err("sync object"))?;
            }
        }
        match fs::rename(&tmp, &path) {
            Ok(()) => {
                if self.durability == Durability::OnFlush {
                    self.pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(path);
                }
                Ok(id)
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                // A concurrent writer that got there first stored the same
                // bytes, by definition — the name is the hash.
                if path.exists() {
                    Ok(id)
                } else {
                    Err(io_err("publish object")(e))
                }
            }
        }
    }

    fn has_object(&self, id: &ObjectId) -> Result<bool> {
        Ok(self.loose_path(id).exists())
    }

    fn get_object(&self, id: &ObjectId) -> Result<StoredObject> {
        let Some((kind, bytes)) = self.read_loose(id)? else {
            return Err(ObjectError::NotFound(id.to_hex()));
        };
        let actual = ObjectId::compute(kind, &bytes);
        if actual != *id {
            return Err(ObjectError::IdMismatch {
                expected: id.to_hex(),
                actual: actual.to_hex(),
            });
        }
        Ok(StoredObject {
            id: *id,
            kind,
            bytes,
        })
    }

    fn verify_object(&self, id: &ObjectId) -> Result<VerifyResult> {
        let object = self.get_object(id)?;
        // Reading the envelope back is part of verification: bytes that hash
        // correctly can still be structurally undecodable if they were
        // written by a broken producer.
        let envelope = object.envelope(self.limits)?;
        Ok(VerifyResult {
            id: object.id,
            kind: object.kind,
            stored_len: object.bytes.len(),
            dependencies: envelope.dependencies.len(),
        })
    }
}

/// Answering graph questions straight off the filesystem.
///
/// A payload object is resolved from its 14-byte header and the file's size:
/// a chunk has no dependencies by construction, so reading megabytes of it to
/// learn that would be waste. Only a structural object is read in full, and
/// only structural objects are ever large in count rather than in bytes.
impl GraphSource for FsObjectStore {
    fn lookup(&self, id: &ObjectId) -> Result<Option<ObjectNode>> {
        let path = self.loose_path(id);
        let mut file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err("open object")(e)),
        };
        let stored_len = file
            .metadata()
            .map_err(io_err("stat object"))?
            .len()
            .saturating_sub(LOOSE_HEADER_LEN as u64);

        let mut header = [0u8; LOOSE_HEADER_LEN];
        file.read_exact(&mut header)
            .map_err(|_| ObjectError::Truncated("loose object header"))?;
        if &header[..8] != LOOSE_MAGIC {
            return Err(ObjectError::Corrupt(format!(
                "object {id} has a bad file header"
            )));
        }
        let version = u16::from_le_bytes([header[8], header[9]]);
        if version != STORE_FORMAT_V1 {
            return Err(ObjectError::UnsupportedFormat(version));
        }
        let tag = u32::from_le_bytes([header[10], header[11], header[12], header[13]]);
        let kind = ObjectKind::from_tag(tag).ok_or(ObjectError::UnknownKind(tag))?;

        let dependencies = if kind.is_payload() {
            Vec::new()
        } else {
            let mut bytes = Vec::with_capacity(stored_len as usize);
            file.read_to_end(&mut bytes)
                .map_err(io_err("read object"))?;
            ObjectEnvelope::decode(kind, &bytes, self.limits)?.dependencies
        };

        Ok(Some(ObjectNode {
            id: *id,
            kind,
            stored_len,
            dependencies,
        }))
    }
}

fn io_err(what: &'static str) -> impl Fn(std::io::Error) -> ObjectError {
    move |source| ObjectError::Io { what, source }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, FsObjectStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path().join("objects")).unwrap();
        (dir, store)
    }

    #[test]
    fn put_then_get_round_trips() {
        let (_dir, store) = store();
        let env = ObjectEnvelope::leaf(ObjectKind::Tree, b"tree body".to_vec()).unwrap();
        let id = store.put_envelope(&env).unwrap();
        assert_eq!(id, env.id());
        assert!(store.has_object(&id).unwrap());
        let got = store.get_object(&id).unwrap();
        assert_eq!(got.kind, ObjectKind::Tree);
        assert_eq!(got.envelope(DecodeLimits::DEFAULT).unwrap(), env);
    }

    #[test]
    fn writing_twice_stores_one_object() {
        let (_dir, store) = store();
        let a = store.put_object(ObjectKind::Commit, b"same").unwrap();
        let b = store.put_object(ObjectKind::Commit, b"same").unwrap();
        assert_eq!(a, b);
        assert_eq!(store.object_count().unwrap(), 1);
    }

    #[test]
    fn the_same_bytes_under_two_kinds_are_two_objects() {
        let (_dir, store) = store();
        let tree = store.put_object(ObjectKind::Tree, b"bytes").unwrap();
        let commit = store.put_object(ObjectKind::Commit, b"bytes").unwrap();
        assert_ne!(tree, commit);
        assert_eq!(store.object_count().unwrap(), 2);
        assert_eq!(store.get_object(&tree).unwrap().kind, ObjectKind::Tree);
        assert_eq!(store.get_object(&commit).unwrap().kind, ObjectKind::Commit);
    }

    #[test]
    fn a_missing_object_is_not_found() {
        let (_dir, store) = store();
        let ghost = ObjectId::from_blake3([3; 32]);
        assert!(!store.has_object(&ghost).unwrap());
        assert!(matches!(
            store.get_object(&ghost),
            Err(ObjectError::NotFound(_))
        ));
    }

    /// A corrupt object must never reach the caller as a value.
    #[test]
    fn flipping_a_byte_on_disk_is_caught() {
        let (_dir, store) = store();
        let id = store
            .put_object(ObjectKind::Chunk, b"payload that will rot")
            .unwrap();
        let path = store.loose_path(&id);
        let mut raw = fs::read(&path).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0xff;
        fs::write(&path, &raw).unwrap();

        assert!(matches!(
            store.get_object(&id),
            Err(ObjectError::IdMismatch { .. })
        ));
        assert!(store.verify_object(&id).is_err());
        let report = store.verify_all().unwrap();
        assert!(!report.is_clean());
        assert_eq!(report.failures.len(), 1);
    }

    /// Rewriting the kind byte in the header cannot make the store hand back
    /// an object under a class it was not hashed as.
    #[test]
    fn rewriting_the_stored_kind_is_caught() {
        let (_dir, store) = store();
        let id = store.put_object(ObjectKind::Tree, b"x").unwrap();
        let path = store.loose_path(&id);
        let mut raw = fs::read(&path).unwrap();
        raw[10..14].copy_from_slice(&ObjectKind::Commit.tag().to_le_bytes());
        fs::write(&path, &raw).unwrap();
        assert!(matches!(
            store.get_object(&id),
            Err(ObjectError::IdMismatch { .. })
        ));
    }

    #[test]
    fn verify_reports_the_dependency_count() {
        let (_dir, store) = store();
        let env = ObjectEnvelope::new(
            ObjectKind::Commit,
            vec![
                ObjectId::from_blake3([1; 32]),
                ObjectId::from_blake3([2; 32]),
            ],
            b"body".to_vec(),
        )
        .unwrap();
        let id = store.put_envelope(&env).unwrap();
        let result = store.verify_object(&id).unwrap();
        assert_eq!(result.dependencies, 2);
        assert_eq!(result.kind, ObjectKind::Commit);
    }

    #[test]
    fn listing_is_id_ordered_and_complete() {
        let (_dir, store) = store();
        let mut expected: Vec<ObjectId> = (0..32u8)
            .map(|i| store.put_object(ObjectKind::Tree, &[i]).unwrap())
            .collect();
        expected.sort();
        assert_eq!(store.list_objects().unwrap(), expected);
    }

    #[test]
    fn removal_is_reported_once() {
        let (_dir, store) = store();
        let id = store.put_object(ObjectKind::Tree, b"gone").unwrap();
        assert!(store.remove_object(&id).unwrap());
        assert!(!store.remove_object(&id).unwrap());
        assert!(!store.has_object(&id).unwrap());
    }

    #[test]
    fn reopening_sees_what_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("objects");
        let id = {
            let store = FsObjectStore::open(&path).unwrap();
            store.put_object(ObjectKind::Tree, b"durable").unwrap()
        };
        let reopened = FsObjectStore::open(&path).unwrap();
        assert!(reopened.has_object(&id).unwrap());
        assert_eq!(reopened.get_object(&id).unwrap().bytes, b"durable");
    }

    /// A crash between the temporary write and the rename leaves a stray file
    /// in tmp/, never a half-written object under its final name.
    #[test]
    fn flushing_forces_what_was_written() {
        let (_dir, store) = store();
        assert_eq!(store.durability(), Durability::OnFlush);
        let a = store.put_object(ObjectKind::Tree, b"one").unwrap();
        let b = store.put_object(ObjectKind::Tree, b"two").unwrap();
        assert_eq!(store.flush().unwrap(), 2);
        // A second flush has nothing left to force.
        assert_eq!(store.flush().unwrap(), 0);
        assert!(store.has_object(&a).unwrap());
        assert!(store.has_object(&b).unwrap());
    }

    #[test]
    fn flushing_tolerates_an_object_that_went_away() {
        let (_dir, store) = store();
        let id = store.put_object(ObjectKind::Tree, b"transient").unwrap();
        assert!(store.remove_object(&id).unwrap());
        assert_eq!(store.flush().unwrap(), 0);
    }

    #[test]
    fn a_store_that_never_syncs_still_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path().join("objects"))
            .unwrap()
            .with_durability(Durability::Never);
        let id = store.put_object(ObjectKind::Tree, b"scratch").unwrap();
        assert_eq!(store.flush().unwrap(), 0);
        assert_eq!(store.get_object(&id).unwrap().bytes, b"scratch");
    }

    #[test]
    fn a_stray_temporary_is_not_an_object() {
        let (_dir, store) = store();
        fs::write(store.root().join("tmp").join("half-written"), b"garbage").unwrap();
        assert_eq!(store.object_count().unwrap(), 0);
        assert!(store.verify_all().unwrap().is_clean());
    }
}
