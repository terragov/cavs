//! Packfile physical storage (`.cavspack` + `.cavsindex`, v0.4.0).
//!
//! Object-per-chunk storage is simple but operationally expensive at scale:
//! a 570 MB build is ~6,000 small files, which means slow directory walks,
//! poor read locality and one open/read syscall pair per served chunk.
//! Packfiles store the same stored (possibly zstd) chunk bytes appended
//! into a few large immutable files, read back by range.
//!
//! ## `.cavspack` layout
//!
//! ```text
//! Header (16 bytes):
//!   magic          8 bytes  "CAVSPK1\0"
//!   version_major  u16 LE   1
//!   version_minor  u16 LE   0
//!   flags          u32 LE   reserved (0)
//! Chunk data region:
//!   concatenated stored chunk bytes (no per-chunk framing; boundaries
//!   live in the index)
//! Footer (40 bytes):
//!   magic          8 bytes  "CAVSPEND"
//!   pack_hash      [32]     BLAKE3 of every byte before the footer
//! ```
//!
//! The pack id is the BLAKE3 of the *entire file* (header + data + footer),
//! so packs are content-addressed: the filename is derived from the id and
//! a pack can never change after creation — ideal for CDN caching and
//! object storage. Chunks are written in the order the manifest references
//! them (reconstruction order), so update fetches touch mostly-contiguous
//! ranges that coalesce well.
//!
//! ## `.cavsindex` sidecar
//!
//! One per pack, written at close: the chunk table needed to read the pack
//! without the store ledger (recovery, `store export`).
//!
//! ```text
//!   magic          8 bytes  "CAVSIDX1"
//!   pack_id        [32]
//!   entry_count    u32 LE
//!   entry_count × {
//!     hash        [32]
//!     offset      u64 LE   // into the pack's data region (absolute file
//!                          // offset = HEADER_LEN + offset)
//!     stored_len  u32 LE
//!     raw_len     u32 LE
//!     flags       u32 LE
//!   }
//!   body_hash      [32]    BLAKE3 of every byte before this field
//! ```

use crate::{Result, StoreError};
use cavs_hash::{hash_chunk, to_hex, ChunkHash, Hasher};
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const PACK_MAGIC: [u8; 8] = *b"CAVSPK1\0";
pub const PACK_FOOTER_MAGIC: [u8; 8] = *b"CAVSPEND";
pub const INDEX_MAGIC: [u8; 8] = *b"CAVSIDX1";
pub const PACK_HEADER_LEN: u64 = 16;
pub const PACK_FOOTER_LEN: u64 = 40;

/// Default pack-size policy (bytes). A pack is closed once its data region
/// reaches the preferred size; small stores end up with a single pack.
pub const PREFERRED_PACK_SIZE: u64 = 128 * 1024 * 1024;

/// One chunk's location inside a pack, as recorded in the sidecar index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackEntry {
    pub hash: ChunkHash,
    /// Offset into the pack's data region (file offset − header).
    pub offset: u64,
    pub stored_len: u32,
    pub raw_len: u32,
    pub flags: u32,
}

/// Streaming writer for one `.cavspack`. Bytes are hashed as they are
/// written; `finish()` appends the footer, derives the content-addressed
/// pack id and renames the temp file into `packs/<ab>/<hex>.cavspack`.
pub struct PackWriter {
    out: BufWriter<File>,
    tmp_path: PathBuf,
    packs_dir: PathBuf,
    hasher: Hasher,
    data_len: u64,
    entries: Vec<PackEntry>,
}

impl PackWriter {
    pub fn create(packs_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(packs_dir)?;
        let tmp_path = packs_dir.join(format!("ingest-{}.cavspack.part", std::process::id()));
        let mut out = BufWriter::new(File::create(&tmp_path)?);
        let mut header = Vec::with_capacity(PACK_HEADER_LEN as usize);
        header.extend_from_slice(&PACK_MAGIC);
        header.extend_from_slice(&1u16.to_le_bytes());
        header.extend_from_slice(&0u16.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        out.write_all(&header)?;
        let mut hasher = Hasher::new();
        hasher.update(&header);
        Ok(Self {
            out,
            tmp_path,
            packs_dir: packs_dir.to_path_buf(),
            hasher,
            data_len: 0,
            entries: Vec::new(),
        })
    }

    /// Append one stored chunk; returns its offset in the data region.
    pub fn append(
        &mut self,
        hash: ChunkHash,
        stored: &[u8],
        raw_len: u32,
        flags: u32,
    ) -> Result<u64> {
        let offset = self.data_len;
        self.out.write_all(stored)?;
        self.hasher.update(stored);
        self.data_len += stored.len() as u64;
        self.entries.push(PackEntry {
            hash,
            offset,
            stored_len: stored.len() as u32,
            raw_len,
            flags,
        });
        Ok(offset)
    }

    pub fn data_len(&self) -> u64 {
        self.data_len
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Close the pack: footer, content-addressed rename, sidecar index.
    /// Returns the pack id (hex) and the recorded entries.
    pub fn finish(mut self) -> Result<(String, Vec<PackEntry>)> {
        let mut footer = Vec::with_capacity(PACK_FOOTER_LEN as usize);
        footer.extend_from_slice(&PACK_FOOTER_MAGIC);
        footer.extend_from_slice(&self.hasher.finalize());
        self.out.write_all(&footer)?;
        self.out.flush()?;
        let file = self.out.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        drop(file);

        // Pack id = BLAKE3 of the whole file (streamed; packs can be large).
        let pack_id = hash_file(&self.tmp_path)?;
        let hex = to_hex(&pack_id);
        let final_path = pack_path(&self.packs_dir, &hex);
        std::fs::create_dir_all(final_path.parent().unwrap())?;
        std::fs::rename(&self.tmp_path, &final_path)?;

        write_pack_index(&index_path(&self.packs_dir, &hex), &pack_id, &self.entries)?;
        Ok((hex, self.entries))
    }

    /// Abort: remove the temp file (crash-safety is handled by callers
    /// ignoring `*.part` files on open).
    pub fn abort(self) {
        drop(self.out);
        let _ = std::fs::remove_file(&self.tmp_path);
    }
}

/// `packs/<ab>/<hex>.cavspack` — prefix split keeps directories small.
pub fn pack_path(packs_dir: &Path, pack_hex: &str) -> PathBuf {
    packs_dir
        .join(&pack_hex[..2])
        .join(format!("{pack_hex}.cavspack"))
}

/// The pack's sidecar index, next to it.
pub fn index_path(packs_dir: &Path, pack_hex: &str) -> PathBuf {
    packs_dir
        .join(&pack_hex[..2])
        .join(format!("{pack_hex}.cavsindex"))
}

/// Read a range of a pack's data region (offset is data-relative).
pub fn read_pack_range(pack_file: &Path, offset: u64, len: u64) -> Result<Vec<u8>> {
    let mut file = File::open(pack_file)?;
    let file_len = file.metadata()?.len();
    let abs = PACK_HEADER_LEN + offset;
    if file_len < PACK_HEADER_LEN + PACK_FOOTER_LEN
        || abs > file_len - PACK_FOOTER_LEN
        || len > file_len - PACK_FOOTER_LEN - abs
    {
        return Err(StoreError::PackCorrupt(format!(
            "{}: range {offset}+{len} outside data region",
            pack_file.display()
        )));
    }
    file.seek(SeekFrom::Start(abs))?;
    let mut buf = vec![0u8; len as usize];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

/// Verify a pack's header magic and footer hash by streaming the file.
pub fn verify_pack(pack_file: &Path) -> Result<()> {
    let mut file = File::open(pack_file)?;
    let file_len = file.metadata()?.len();
    if file_len < PACK_HEADER_LEN + PACK_FOOTER_LEN {
        return Err(StoreError::PackCorrupt(format!(
            "{}: too short",
            pack_file.display()
        )));
    }
    let mut hasher = Hasher::new();
    let mut remaining = file_len - PACK_FOOTER_LEN;
    let mut buf = vec![0u8; 1 << 20];
    let mut first = true;
    while remaining > 0 {
        let n = remaining.min(buf.len() as u64) as usize;
        file.read_exact(&mut buf[..n])?;
        if first {
            if buf[..PACK_MAGIC.len().min(n)] != PACK_MAGIC[..PACK_MAGIC.len().min(n)] {
                return Err(StoreError::PackCorrupt(format!(
                    "{}: bad magic",
                    pack_file.display()
                )));
            }
            first = false;
        }
        hasher.update(&buf[..n]);
        remaining -= n as u64;
    }
    let mut footer = [0u8; PACK_FOOTER_LEN as usize];
    file.read_exact(&mut footer)?;
    if footer[..8] != PACK_FOOTER_MAGIC || footer[8..] != hasher.finalize() {
        return Err(StoreError::PackCorrupt(format!(
            "{}: footer hash mismatch",
            pack_file.display()
        )));
    }
    Ok(())
}

/// Bytes per index entry: hash(32) + offset(8) + stored/raw/flags (3×4).
const INDEX_ENTRY_LEN: usize = 52;

fn write_pack_index(path: &Path, pack_id: &ChunkHash, entries: &[PackEntry]) -> Result<()> {
    let mut body = Vec::with_capacity(48 + entries.len() * INDEX_ENTRY_LEN);
    body.extend_from_slice(&INDEX_MAGIC);
    body.extend_from_slice(pack_id);
    body.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        body.extend_from_slice(&e.hash);
        body.extend_from_slice(&e.offset.to_le_bytes());
        body.extend_from_slice(&e.stored_len.to_le_bytes());
        body.extend_from_slice(&e.raw_len.to_le_bytes());
        body.extend_from_slice(&e.flags.to_le_bytes());
    }
    let digest = hash_chunk(&body);
    body.extend_from_slice(&digest);
    let tmp = path.with_extension("cavsindex.tmp");
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Read and validate a `.cavsindex`: returns (pack id, entries).
pub fn read_pack_index(path: &Path) -> Result<(ChunkHash, Vec<PackEntry>)> {
    let bytes = std::fs::read(path)?;
    let corrupt = |what: &str| StoreError::PackCorrupt(format!("{}: {what}", path.display()));
    if bytes.len() < 44 + 32 || bytes[..8] != INDEX_MAGIC {
        return Err(corrupt("bad index header"));
    }
    let (body, digest) = bytes.split_at(bytes.len() - 32);
    if hash_chunk(body) != *digest {
        return Err(corrupt("index hash mismatch"));
    }
    let pack_id: ChunkHash = body[8..40].try_into().unwrap();
    let count = u32::from_le_bytes(body[40..44].try_into().unwrap()) as usize;
    let entries_bytes = &body[44..];
    if entries_bytes.len() != count * INDEX_ENTRY_LEN {
        return Err(corrupt("index entry count mismatch"));
    }
    let mut entries = Vec::with_capacity(count);
    for e in entries_bytes.chunks_exact(INDEX_ENTRY_LEN) {
        entries.push(PackEntry {
            hash: e[..32].try_into().unwrap(),
            offset: u64::from_le_bytes(e[32..40].try_into().unwrap()),
            stored_len: u32::from_le_bytes(e[40..44].try_into().unwrap()),
            raw_len: u32::from_le_bytes(e[44..48].try_into().unwrap()),
            flags: u32::from_le_bytes(e[48..52].try_into().unwrap()),
        });
    }
    Ok((pack_id, entries))
}

fn hash_file(path: &Path) -> Result<ChunkHash> {
    let mut file = File::open(path)?;
    let mut hasher = Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(i: u32) -> Vec<u8> {
        vec![i as u8; 1000 + i as usize]
    }

    #[test]
    fn pack_roundtrip_and_verify() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = PackWriter::create(dir.path()).unwrap();
        let mut expected = Vec::new();
        for i in 0..20u32 {
            let data = chunk(i);
            let hash = hash_chunk(&data);
            let offset = writer.append(hash, &data, data.len() as u32, 0).unwrap();
            expected.push((hash, offset, data));
        }
        let (pack_hex, entries) = writer.finish().unwrap();
        assert_eq!(entries.len(), 20);

        let pack = pack_path(dir.path(), &pack_hex);
        assert!(pack.is_file());
        verify_pack(&pack).unwrap();

        // Every chunk reads back exactly by range.
        for (hash, offset, data) in &expected {
            let bytes = read_pack_range(&pack, *offset, data.len() as u64).unwrap();
            assert_eq!(&bytes, data);
            assert_eq!(hash_chunk(&bytes), *hash);
        }

        // Sidecar index matches what the writer recorded.
        let (pack_id, read_entries) = read_pack_index(&index_path(dir.path(), &pack_hex)).unwrap();
        assert_eq!(to_hex(&pack_id), pack_hex);
        assert_eq!(read_entries, entries);
    }

    #[test]
    fn pack_is_content_addressed_and_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let write = |sub: &str| {
            let d = dir.path().join(sub);
            let mut w = PackWriter::create(&d).unwrap();
            for i in 0..5u32 {
                let data = chunk(i);
                w.append(hash_chunk(&data), &data, data.len() as u32, 0)
                    .unwrap();
            }
            w.finish().unwrap().0
        };
        assert_eq!(
            write("a"),
            write("b"),
            "same content must yield same pack id"
        );
    }

    #[test]
    fn corruption_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = PackWriter::create(dir.path()).unwrap();
        let data = chunk(1);
        writer
            .append(hash_chunk(&data), &data, data.len() as u32, 0)
            .unwrap();
        let (pack_hex, entries) = writer.finish().unwrap();
        let pack = pack_path(dir.path(), &pack_hex);

        // Flip one payload byte: pack verification fails, and the chunk read
        // no longer matches its identity hash.
        let mut bytes = std::fs::read(&pack).unwrap();
        bytes[PACK_HEADER_LEN as usize + 10] ^= 0xff;
        std::fs::write(&pack, &bytes).unwrap();
        assert!(verify_pack(&pack).is_err());
        let read = read_pack_range(&pack, entries[0].offset, entries[0].stored_len as u64).unwrap();
        assert_ne!(hash_chunk(&read), entries[0].hash);

        // Truncation: range reads outside the data region are rejected.
        let truncated = &bytes[..bytes.len() - 60];
        std::fs::write(&pack, truncated).unwrap();
        assert!(verify_pack(&pack).is_err());
        assert!(read_pack_range(&pack, entries[0].offset, entries[0].stored_len as u64).is_err());
    }

    #[test]
    fn corrupt_index_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = PackWriter::create(dir.path()).unwrap();
        let data = chunk(2);
        writer
            .append(hash_chunk(&data), &data, data.len() as u32, 0)
            .unwrap();
        let (pack_hex, _) = writer.finish().unwrap();
        let idx = index_path(dir.path(), &pack_hex);
        let mut bytes = std::fs::read(&idx).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0x01;
        std::fs::write(&idx, &bytes).unwrap();
        assert!(read_pack_index(&idx).is_err());
    }
}
