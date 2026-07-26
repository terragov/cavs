//! CAVS-1 binary container format: types, writer and reader.
//!
//! CAVS-1 (Content-Addressable Versioned Streaming, v1) is a packaging layer
//! for large files (builds, models, datasets, images, packs, bundles, patches)
//! — and, secondarily, video. It stores deduplicated, content-hashed chunks
//! plus the tables needed
//! to reconstruct the original files byte-for-byte (raw assets, or fMP4/CMAF
//! segments and playlists when packaging video).
//!
//! On-disk layout (little-endian throughout):
//!
//! ```text
//! +-----------------------------+ offset 0
//! | Superblock (64 bytes)       |
//! +-----------------------------+ offset 64
//! | DATA section (chunk bytes)  |  streamed while packing
//! +-----------------------------+
//! | TRACKS section              |
//! | DICT section                |
//! | CHUNKS section              |
//! | SEGMENTS section            |
//! | META section                |
//! | INTEGRITY section           |
//! +-----------------------------+
//! | Section directory           |  pointed to by the superblock
//! +-----------------------------+
//! ```
//!
//! See `FORMAT.md` at the workspace root for the full byte-level spec.

mod ingest;
mod reader;
mod writer;

pub use ingest::{ingest_into_store, IngestStats};
pub use reader::{Reader, SignatureStatus, VerifyReport};
pub use writer::{PackStats, Writer};

use cavs_hash::ChunkHash;

/// File magic: "CAVS".
pub const MAGIC: [u8; 4] = *b"CAVS";
pub const VERSION_MAJOR: u16 = 1;
pub const VERSION_MINOR: u16 = 0;
/// Fixed superblock size in bytes.
pub const SUPERBLOCK_LEN: u64 = 64;
/// Size of one section-directory entry: type(4) + offset(8) + len(8) + hash(32).
pub const SECTION_DIR_ENTRY_LEN: usize = 52;
/// Upper bound on a single chunk's uncompressed size (256 MiB). Larger than
/// any chunker's max; used to cap decompression allocations from untrusted
/// or corrupted files.
pub const MAX_CHUNK_RAW: u64 = 256 * 1024 * 1024;

/// Section identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum SectionType {
    Tracks = 1,
    Dict = 2,
    Chunks = 3,
    Segments = 4,
    Data = 5,
    Integrity = 6,
    Meta = 7,
}

impl SectionType {
    pub fn from_u32(v: u32) -> Option<Self> {
        Some(match v {
            1 => SectionType::Tracks,
            2 => SectionType::Dict,
            3 => SectionType::Chunks,
            4 => SectionType::Segments,
            5 => SectionType::Data,
            6 => SectionType::Integrity,
            7 => SectionType::Meta,
            _ => return None,
        })
    }
}

/// Compression algorithm ids (superblock default and per-chunk flag).
pub const COMPRESSION_NONE: u8 = 0;
pub const COMPRESSION_ZSTD: u8 = 1;

/// Chunk flag bit: payload stored zstd-compressed.
pub const CHUNK_FLAG_ZSTD: u32 = 1 << 0;

/// Chunk flag bit: payload ran through the BG4 byte-grouping pretransform
/// before compression (stored bytes are `zstd(bg4_group(raw))`). Always set
/// together with [`CHUNK_FLAG_ZSTD`].
pub const CHUNK_FLAG_BG4: u32 = 1 << 1;

/// Byte-grouping-of-4 pretransform (BG4): scatter the payload into four
/// planes by byte index mod 4 (`raw[0], raw[4], …` then `raw[1], raw[5], …`
/// and so on). Little-endian f32/i32 streams — model weights, vertex
/// buffers, audio samples — put each position's slowly-varying bytes next to
/// each other, which zstd compresses far better than the interleaved
/// original. Length-preserving; inverted by [`bg4_ungroup`].
pub fn bg4_group(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    for lane in 0..4 {
        out.extend(raw.iter().skip(lane).step_by(4));
    }
    out
}

/// Inverse of [`bg4_group`].
pub fn bg4_ungroup(grouped: &[u8]) -> Vec<u8> {
    let len = grouped.len();
    let mut out = vec![0u8; len];
    let mut it = grouped.iter();
    for lane in 0..4 {
        let mut i = lane;
        while i < len {
            out[i] = *it.next().unwrap();
            i += 4;
        }
    }
    out
}

/// Segment flag bit: random-access point (keyframe bundle boundary).
pub const SEGMENT_FLAG_RANDOM_ACCESS: u32 = 1 << 0;

/// Track kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TrackKind {
    Video = 0,
    Audio = 1,
    Subtitle = 2,
    /// Auxiliary binary asset (playlists, raw files, textures, ...).
    Data = 3,
}

impl TrackKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => TrackKind::Video,
            1 => TrackKind::Audio,
            2 => TrackKind::Subtitle,
            3 => TrackKind::Data,
            _ => return None,
        })
    }

    pub fn label(&self) -> &'static str {
        match self {
            TrackKind::Video => "video",
            TrackKind::Audio => "audio",
            TrackKind::Subtitle => "subtitle",
            TrackKind::Data => "data",
        }
    }
}

/// Superblock contents.
#[derive(Debug, Clone)]
pub struct Superblock {
    pub version_major: u16,
    pub version_minor: u16,
    pub feature_flags: u32,
    pub hash_algo: u8,
    pub compression_algo: u8,
    pub asset_uuid: [u8; 16],
    pub timescale: u32,
    pub section_count: u32,
    pub section_dir_offset: u64,
    pub file_size: u64,
}

/// One entry of the section directory.
#[derive(Debug, Clone)]
pub struct SectionEntry {
    pub section_type: SectionType,
    pub offset: u64,
    pub length: u64,
    /// blake3 of the section's raw bytes.
    pub hash: ChunkHash,
}

/// Chunk-table record. `data_offset` is relative to the DATA section start.
#[derive(Debug, Clone)]
pub struct ChunkRecord {
    pub hash: ChunkHash,
    pub data_offset: u64,
    pub len_raw: u32,
    pub len_stored: u32,
    pub flags: u32,
}

/// Track-table record.
#[derive(Debug, Clone)]
pub struct TrackRecord {
    pub track_id: u32,
    pub kind: TrackKind,
    pub flags: u8,
    pub codec: String,
    /// Logical name, e.g. original file/segment naming hint.
    pub name: String,
    pub timescale: u32,
    /// Chunks of the track init payload (e.g. CMAF init segment), in order.
    pub init_chunks: Vec<u32>,
}

/// Segment-directory record. Reconstruction of the segment payload is the
/// ordered concatenation of the raw bytes of `chunks`.
#[derive(Debug, Clone)]
pub struct SegmentRecord {
    pub segment_id: u64,
    pub track_id: u32,
    pub pts_start: u64,
    pub duration: u32,
    pub flags: u32,
    pub chunks: Vec<u32>,
}

/// Integrity section contents.
#[derive(Debug, Clone)]
pub struct Integrity {
    /// Merkle root over the chunk table's hashes, in table order.
    pub merkle_root: ChunkHash,
    pub chunk_count: u64,
    pub total_raw: u64,
    pub total_stored: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("not a CAVS file (bad magic)")]
    BadMagic,
    #[error("unsupported CAVS major version {0}")]
    UnsupportedVersion(u16),
    #[error("truncated or malformed {0} section")]
    Malformed(&'static str),
    #[error("missing required section {0:?}")]
    MissingSection(SectionType),
    #[error("unknown enum value {value} for {what}")]
    UnknownValue { what: &'static str, value: u32 },
    #[error("chunk {index} hash mismatch (corrupted payload)")]
    ChunkHashMismatch { index: u32 },
    #[error("section {0:?} hash mismatch (corrupted section)")]
    SectionHashMismatch(SectionType),
    #[error("merkle root mismatch (chunk table tampered)")]
    MerkleMismatch,
    #[error("embedded content signature is invalid")]
    SignatureInvalid,
    #[error("chunk index {0} out of range")]
    ChunkIndexOutOfRange(u32),
    #[error("track {0} not found")]
    TrackNotFound(u32),
    #[error("zstd error: {0}")]
    Zstd(std::io::Error),
    #[error("store error: {0}")]
    Store(#[from] cavs_store::StoreError),
}

pub type Result<T> = std::result::Result<T, FormatError>;

// ---------------------------------------------------------------------------
// Little-endian encode/decode helpers shared by reader and writer.
// ---------------------------------------------------------------------------

pub(crate) mod wire {
    use super::{FormatError, Result};

    pub fn put_u16(buf: &mut Vec<u8>, v: u16) {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn put_u32(buf: &mut Vec<u8>, v: u32) {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn put_u64(buf: &mut Vec<u8>, v: u64) {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    /// String with u16 length prefix.
    pub fn put_str(buf: &mut Vec<u8>, s: &str) {
        let bytes = s.as_bytes();
        assert!(bytes.len() <= u16::MAX as usize, "string too long for wire");
        put_u16(buf, bytes.len() as u16);
        buf.extend_from_slice(bytes);
    }
    /// Bytes with u32 length prefix.
    pub fn put_bytes32(buf: &mut Vec<u8>, b: &[u8]) {
        put_u32(buf, b.len() as u32);
        buf.extend_from_slice(b);
    }

    /// Sequential decoder over a byte slice.
    pub struct Cursor<'a> {
        buf: &'a [u8],
        pos: usize,
        what: &'static str,
    }

    impl<'a> Cursor<'a> {
        pub fn new(buf: &'a [u8], what: &'static str) -> Self {
            Self { buf, pos: 0, what }
        }

        fn take(&mut self, n: usize) -> Result<&'a [u8]> {
            if self.pos + n > self.buf.len() {
                return Err(FormatError::Malformed(self.what));
            }
            let s = &self.buf[self.pos..self.pos + n];
            self.pos += n;
            Ok(s)
        }

        pub fn u8(&mut self) -> Result<u8> {
            Ok(self.take(1)?[0])
        }
        pub fn u16(&mut self) -> Result<u16> {
            Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
        }
        pub fn u32(&mut self) -> Result<u32> {
            Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
        }
        pub fn u64(&mut self) -> Result<u64> {
            Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
        }
        pub fn hash(&mut self) -> Result<[u8; 32]> {
            Ok(self.take(32)?.try_into().unwrap())
        }
        pub fn str16(&mut self) -> Result<String> {
            let len = self.u16()? as usize;
            let bytes = self.take(len)?;
            String::from_utf8(bytes.to_vec()).map_err(|_| FormatError::Malformed(self.what))
        }
        pub fn bytes32(&mut self) -> Result<Vec<u8>> {
            let len = self.u32()? as usize;
            Ok(self.take(len)?.to_vec())
        }
    }
}
