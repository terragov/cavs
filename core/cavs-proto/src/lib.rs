//! CAVS-1 streaming protocol types.
//!
//! Control plane (manifest, sessions) travels as JSON; the data plane
//! (chunk batches) uses a compact binary encoding, `CVSP` v2:
//!
//! ```text
//! "CVSP" u8 version=2
//! u32 init_count
//!   init_count × { u32 track_id; u32 n; n × instr }
//! u32 segment_count
//!   segment_count × { u64 segment_id; u32 n; n × instr }
//! instr:
//!   u8 tag            // 0 = Ref (client already has it), 1 = Inline
//!   [32] hash
//!   if Inline:
//!     u8  compression  // 0 = none, 1 = zstd
//!     u32 len_raw      // uncompressed length
//!     u32 len_stored   // payload length on the wire
//!     len_stored × u8  // payload (as stored: possibly zstd-compressed)
//! ```
//!
//! Inline payloads travel exactly as stored in the `.cavs` DATA section, so
//! the origin never recompresses. `hash` always refers to the *uncompressed*
//! bytes: receivers decompress first, then check `blake3(raw) == hash`.

pub mod errors;

use cavs_hash::ChunkHash;
use serde::{Deserialize, Serialize};

pub const BATCH_MAGIC: [u8; 4] = *b"CVSP";
pub const BATCH_VERSION: u8 = 2;

/// Inline payload compression ids.
pub const WIRE_COMPRESSION_NONE: u8 = 0;
pub const WIRE_COMPRESSION_ZSTD: u8 = 1;

/// Hard ceiling on one inline chunk's wire/raw length (matches the
/// container's chunk limit). Real chunks are ≤ a few MiB; anything above
/// this is a malformed or hostile stream, rejected before allocating.
pub const MAX_WIRE_CHUNK: u32 = 256 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Control plane (JSON)
// ---------------------------------------------------------------------------

/// One chunk reference inside a manifest: hex BLAKE3 plus raw length.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkRef {
    pub hash: String,
    pub len: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestTrack {
    pub track_id: u32,
    pub kind: String,
    pub codec: String,
    pub name: String,
    pub timescale: u32,
    pub init_chunks: Vec<ChunkRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestSegment {
    pub segment_id: u64,
    pub track_id: u32,
    pub pts_start: u64,
    pub duration: u32,
    pub random_access: bool,
    pub chunks: Vec<ChunkRef>,
}

/// Full asset manifest: everything a client needs to plan a fetch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub asset: String,
    pub asset_uuid: String,
    pub tracks: Vec<ManifestTrack>,
    pub segments: Vec<ManifestSegment>,
    /// Chunk hashes pinned in the global dictionary (bootstrap payloads).
    pub dict: Vec<String>,
    /// Full chunk-table hashes in table order (the Merkle leaf order), so
    /// clients can recompute the root and check content signatures.
    #[serde(default)]
    pub chunk_table: Vec<String>,
    /// Hex Merkle root over `chunk_table`.
    #[serde(default)]
    pub merkle_root: String,
    /// Hex Ed25519 signature over the CAVS-1 content message, if signed.
    #[serde(default)]
    pub signature: Option<String>,
    /// Hex Ed25519 public key of the signer, if signed.
    #[serde(default)]
    pub signer_pubkey: Option<String>,
    /// Packer meta entries (e.g. per-file `sha256:<name>` digests that thin
    /// clients without BLAKE3 use for end-to-end verification).
    #[serde(default)]
    pub meta: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetSummary {
    pub name: String,
    pub tracks: usize,
    pub segments: usize,
    pub unique_chunks: u64,
}

/// Compact Bloom-filter summary of a client's have-set, so a session open
/// stays small even when the cache holds tens of thousands of chunks (a full
/// hash list would be ~64 bytes per chunk). Membership uses double hashing
/// (Kirsch–Mitzenmacher) over two 64-bit slices of the chunk's BLAKE3 hash;
/// false positives make the server send a `Ref` the client lacks, which the
/// client repairs by fetching that chunk directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BloomFilter {
    /// Number of bits (multiple of 8).
    pub m: u64,
    /// Number of hash probes.
    pub k: u32,
    /// Bit array, `m/8` bytes.
    pub bits: Vec<u8>,
}

impl BloomFilter {
    /// Sized for `n` expected elements at ~1% false-positive rate
    /// (m ≈ 10n bits, k = 7).
    pub fn with_capacity(n: usize) -> Self {
        let m_bits = ((n.max(1) * 10) as u64).next_multiple_of(8).max(64);
        BloomFilter {
            m: m_bits,
            k: 7,
            bits: vec![0u8; (m_bits / 8) as usize],
        }
    }

    fn probes(&self, hash: &ChunkHash) -> impl Iterator<Item = u64> + '_ {
        let h1 = u64::from_le_bytes(hash[0..8].try_into().unwrap());
        let h2 = u64::from_le_bytes(hash[8..16].try_into().unwrap()) | 1;
        let m = self.m;
        (0..self.k as u64).map(move |i| h1.wrapping_add(i.wrapping_mul(h2)) % m)
    }

    pub fn insert(&mut self, hash: &ChunkHash) {
        let probes: Vec<u64> = self.probes(hash).collect();
        for bit in probes {
            self.bits[(bit / 8) as usize] |= 1 << (bit % 8);
        }
    }

    pub fn contains(&self, hash: &ChunkHash) -> bool {
        // A malformed filter (wrong bit length) never claims membership.
        if self.bits.len() as u64 != self.m / 8 || self.m == 0 {
            return false;
        }
        self.probes(hash)
            .all(|bit| self.bits[(bit / 8) as usize] & (1 << (bit % 8)) != 0)
    }
}

/// Session open request. The client sends its have-set either as an exact
/// list of hex hashes (small caches) or as a compact `have_bloom` (large
/// caches). If both are present the bloom is used.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionOpenRequest {
    #[serde(default)]
    pub have: Vec<String>,
    #[serde(default)]
    pub have_bloom: Option<BloomFilter>,
}

/// How the server suggests delivering this asset to this client (v2 dual
/// route). Advisory: a client may always fall back to the chunk path.
pub const DELIVERY_BOOTSTRAP: &str = "bootstrap";
pub const DELIVERY_CHUNKS: &str = "chunks";
pub const DELIVERY_REFERENCES: &str = "references";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionOpenResponse {
    pub session_id: String,
    /// How many of the client's `have` hashes matched this asset.
    pub known_chunks: usize,
    /// Suggested delivery route: `bootstrap` (download the full compressed
    /// artifact and seed the cache locally — cheaper for cold clients),
    /// `chunks` (missing chunks via batches) or `references` (client already
    /// has everything). Absent on v1 servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_mode: Option<String>,
    /// Size in bytes of the bootstrap artifact, when `delivery_mode` is
    /// `bootstrap`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap_size: Option<u64>,
    /// Hex BLAKE3 of the bootstrap artifact bytes (integrity check).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap_blake3: Option<String>,
    /// Estimated wire bytes of the chunk path for this client, so clients
    /// can log/verify the server's routing decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_chunk_payload: Option<u64>,
}

/// Batch request: which track inits and segments to deliver.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchRequest {
    #[serde(default)]
    pub track_inits: Vec<u32>,
    #[serde(default)]
    pub segment_ids: Vec<u64>,
}

// ---------------------------------------------------------------------------
// Data plane (binary)
// ---------------------------------------------------------------------------

/// One delivery instruction for a chunk slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryInstr {
    /// Client already has this chunk (per session have-set): resolve locally.
    Ref { hash: ChunkHash },
    /// Cold chunk: payload included as stored (possibly zstd-compressed).
    Inline {
        hash: ChunkHash,
        /// Uncompressed length (decompression bound and sanity check).
        len_raw: u32,
        /// `WIRE_COMPRESSION_NONE` or `WIRE_COMPRESSION_ZSTD`.
        compression: u8,
        payload: Vec<u8>,
    },
}

impl DeliveryInstr {
    pub fn hash(&self) -> &ChunkHash {
        match self {
            DeliveryInstr::Ref { hash } => hash,
            DeliveryInstr::Inline { hash, .. } => hash,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitDelivery {
    pub track_id: u32,
    pub instrs: Vec<DeliveryInstr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentDelivery {
    pub segment_id: u64,
    pub instrs: Vec<DeliveryInstr>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchResponse {
    pub inits: Vec<InitDelivery>,
    pub segments: Vec<SegmentDelivery>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("bad batch magic or version")]
    BadHeader,
    #[error("truncated or malformed batch payload")]
    Malformed,
    #[error("io error reading batch: {0}")]
    Io(String),
    #[error("batch consumer error: {0}")]
    Consumer(String),
}

impl BatchResponse {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&BATCH_MAGIC);
        out.push(BATCH_VERSION);
        out.extend_from_slice(&(self.inits.len() as u32).to_le_bytes());
        for init in &self.inits {
            out.extend_from_slice(&init.track_id.to_le_bytes());
            encode_instrs(&mut out, &init.instrs);
        }
        out.extend_from_slice(&(self.segments.len() as u32).to_le_bytes());
        for seg in &self.segments {
            out.extend_from_slice(&seg.segment_id.to_le_bytes());
            encode_instrs(&mut out, &seg.instrs);
        }
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        let mut cur = Dec { buf, pos: 0 };
        if cur.take(4)? != BATCH_MAGIC || cur.u8()? != BATCH_VERSION {
            return Err(ProtoError::BadHeader);
        }
        // Counts are untrusted: never pre-allocate more than the buffer
        // could possibly encode (a crafted header must not OOM us).
        let init_count = cur.u32()?;
        let mut inits = Vec::with_capacity((init_count as usize).min(cur.remaining() / 8));
        for _ in 0..init_count {
            let track_id = cur.u32()?;
            inits.push(InitDelivery {
                track_id,
                instrs: decode_instrs(&mut cur)?,
            });
        }
        let seg_count = cur.u32()?;
        let mut segments = Vec::with_capacity((seg_count as usize).min(cur.remaining() / 12));
        for _ in 0..seg_count {
            let segment_id = cur.u64()?;
            segments.push(SegmentDelivery {
                segment_id,
                instrs: decode_instrs(&mut cur)?,
            });
        }
        if cur.pos != buf.len() {
            return Err(ProtoError::Malformed);
        }
        Ok(BatchResponse { inits, segments })
    }
}

/// One item of a streamed batch, in wire order.
#[derive(Debug)]
pub enum BatchItem {
    /// Start of a track-init delivery with `instr_count` instructions.
    Init { track_id: u32, instr_count: u32 },
    /// Start of a segment delivery with `instr_count` instructions.
    Segment { segment_id: u64, instr_count: u32 },
    /// One instruction of the current init/segment. Inline payloads are
    /// handed to the consumer and dropped — nothing accumulates.
    Instr(DeliveryInstr),
}

/// Decode a CVSP batch incrementally from a reader, invoking `on_item` per
/// item. Peak memory is one instruction (≤ one chunk), independent of batch
/// size — use this instead of [`BatchResponse::decode`] when the response
/// body would be large (e.g. a cold install of a full release).
pub fn decode_stream<R: std::io::Read>(
    r: &mut R,
    mut on_item: impl FnMut(BatchItem) -> std::result::Result<(), String>,
) -> Result<(), ProtoError> {
    let io = |e: std::io::Error| ProtoError::Io(e.to_string());
    let mut header = [0u8; 5];
    r.read_exact(&mut header).map_err(io)?;
    if header[..4] != BATCH_MAGIC || header[4] != BATCH_VERSION {
        return Err(ProtoError::BadHeader);
    }

    let mut u32buf = [0u8; 4];
    let mut u64buf = [0u8; 8];
    for section in 0..2u8 {
        r.read_exact(&mut u32buf).map_err(io)?;
        let count = u32::from_le_bytes(u32buf);
        for _ in 0..count {
            let instr_count;
            if section == 0 {
                r.read_exact(&mut u32buf).map_err(io)?;
                let track_id = u32::from_le_bytes(u32buf);
                r.read_exact(&mut u32buf).map_err(io)?;
                instr_count = u32::from_le_bytes(u32buf);
                on_item(BatchItem::Init {
                    track_id,
                    instr_count,
                })
                .map_err(ProtoError::Consumer)?;
            } else {
                r.read_exact(&mut u64buf).map_err(io)?;
                let segment_id = u64::from_le_bytes(u64buf);
                r.read_exact(&mut u32buf).map_err(io)?;
                instr_count = u32::from_le_bytes(u32buf);
                on_item(BatchItem::Segment {
                    segment_id,
                    instr_count,
                })
                .map_err(ProtoError::Consumer)?;
            }
            for _ in 0..instr_count {
                let mut tag_hash = [0u8; 33];
                r.read_exact(&mut tag_hash).map_err(io)?;
                let hash: ChunkHash = tag_hash[1..].try_into().unwrap();
                let instr = match tag_hash[0] {
                    0 => DeliveryInstr::Ref { hash },
                    1 => {
                        let mut meta = [0u8; 9];
                        r.read_exact(&mut meta).map_err(io)?;
                        let compression = meta[0];
                        if compression > WIRE_COMPRESSION_ZSTD {
                            return Err(ProtoError::Malformed);
                        }
                        let len_raw = u32::from_le_bytes(meta[1..5].try_into().unwrap());
                        let len_stored = u32::from_le_bytes(meta[5..9].try_into().unwrap());
                        // Reject hostile lengths before allocating.
                        if len_raw > MAX_WIRE_CHUNK || len_stored > MAX_WIRE_CHUNK {
                            return Err(ProtoError::Malformed);
                        }
                        let mut payload = vec![0u8; len_stored as usize];
                        r.read_exact(&mut payload).map_err(io)?;
                        DeliveryInstr::Inline {
                            hash,
                            len_raw,
                            compression,
                            payload,
                        }
                    }
                    _ => return Err(ProtoError::Malformed),
                };
                on_item(BatchItem::Instr(instr)).map_err(ProtoError::Consumer)?;
            }
        }
    }
    Ok(())
}

fn encode_instrs(out: &mut Vec<u8>, instrs: &[DeliveryInstr]) {
    out.extend_from_slice(&(instrs.len() as u32).to_le_bytes());
    for instr in instrs {
        match instr {
            DeliveryInstr::Ref { hash } => {
                out.push(0);
                out.extend_from_slice(hash);
            }
            DeliveryInstr::Inline {
                hash,
                len_raw,
                compression,
                payload,
            } => {
                out.push(1);
                out.extend_from_slice(hash);
                out.push(*compression);
                out.extend_from_slice(&len_raw.to_le_bytes());
                out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                out.extend_from_slice(payload);
            }
        }
    }
}

fn decode_instrs(cur: &mut Dec) -> Result<Vec<DeliveryInstr>, ProtoError> {
    let n = cur.u32()?;
    // Untrusted count: cap the pre-allocation at what could actually fit
    // (a Ref is the smallest instruction: tag + 32-byte hash).
    let mut instrs = Vec::with_capacity((n as usize).min(cur.remaining() / 33));
    for _ in 0..n {
        let tag = cur.u8()?;
        let hash: ChunkHash = cur.take(32)?.try_into().unwrap();
        match tag {
            0 => instrs.push(DeliveryInstr::Ref { hash }),
            1 => {
                let compression = cur.u8()?;
                if compression > WIRE_COMPRESSION_ZSTD {
                    return Err(ProtoError::Malformed);
                }
                let len_raw = cur.u32()?;
                let len_stored = cur.u32()?;
                if len_raw > MAX_WIRE_CHUNK || len_stored > MAX_WIRE_CHUNK {
                    return Err(ProtoError::Malformed);
                }
                let len_stored = len_stored as usize;
                let payload = cur.take(len_stored)?.to_vec();
                instrs.push(DeliveryInstr::Inline {
                    hash,
                    len_raw,
                    compression,
                    payload,
                });
            }
            _ => return Err(ProtoError::Malformed),
        }
    }
    Ok(instrs)
}

struct Dec<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Dec<'a> {
    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtoError> {
        if n > self.remaining() {
            return Err(ProtoError::Malformed);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, ProtoError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, ProtoError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, ProtoError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cavs_hash::hash_chunk;

    #[test]
    fn batch_roundtrip() {
        let payload = vec![9u8; 1000];
        let resp = BatchResponse {
            inits: vec![InitDelivery {
                track_id: 1,
                instrs: vec![DeliveryInstr::Inline {
                    hash: hash_chunk(&payload),
                    len_raw: payload.len() as u32,
                    compression: WIRE_COMPRESSION_NONE,
                    payload: payload.clone(),
                }],
            }],
            segments: vec![SegmentDelivery {
                segment_id: 42,
                instrs: vec![
                    DeliveryInstr::Ref {
                        hash: hash_chunk(b"warm"),
                    },
                    DeliveryInstr::Inline {
                        hash: hash_chunk(b"cold-raw-bytes"),
                        len_raw: 14,
                        compression: WIRE_COMPRESSION_ZSTD,
                        payload: b"pretend-zstd".to_vec(),
                    },
                ],
            }],
        };
        let decoded = BatchResponse::decode(&resp.encode()).unwrap();
        assert_eq!(decoded, resp);
    }

    #[test]
    fn stream_decoder_matches_buffered() {
        let payload = vec![7u8; 5000];
        let resp = BatchResponse {
            inits: vec![InitDelivery {
                track_id: 3,
                instrs: vec![DeliveryInstr::Ref {
                    hash: hash_chunk(b"x"),
                }],
            }],
            segments: vec![SegmentDelivery {
                segment_id: 9,
                instrs: vec![DeliveryInstr::Inline {
                    hash: hash_chunk(&payload),
                    len_raw: payload.len() as u32,
                    compression: WIRE_COMPRESSION_NONE,
                    payload: payload.clone(),
                }],
            }],
        };
        let encoded = resp.encode();
        let mut items = Vec::new();
        decode_stream(&mut encoded.as_slice(), |item| {
            items.push(format!("{item:?}"));
            Ok(())
        })
        .unwrap();
        assert_eq!(items.len(), 4); // Init + Instr + Segment + Instr
        assert!(items[0].contains("track_id: 3"));
        assert!(items[2].contains("segment_id: 9"));
    }

    #[test]
    fn bloom_membership_no_false_negatives() {
        let members: Vec<ChunkHash> = (0..500u32).map(|i| hash_chunk(&i.to_le_bytes())).collect();
        let mut bf = BloomFilter::with_capacity(members.len());
        for h in &members {
            bf.insert(h);
        }
        // No false negatives: every inserted element must test positive.
        for h in &members {
            assert!(bf.contains(h));
        }
        // False-positive rate on 5000 non-members stays low (~1% target).
        let fp = (500..5500u32)
            .map(|i| hash_chunk(&i.to_le_bytes()))
            .filter(|h| bf.contains(h))
            .count();
        assert!(fp < 200, "false-positive rate too high: {fp}/5000");
        // Survives a JSON round-trip.
        let json = serde_json::to_string(&bf).unwrap();
        let bf2: BloomFilter = serde_json::from_str(&json).unwrap();
        assert!(members.iter().all(|h| bf2.contains(h)));
    }

    #[test]
    fn v1_session_response_still_parses() {
        // A v1 server response has none of the dual-route fields: they must
        // deserialize as None so old servers keep working with new clients.
        let resp: SessionOpenResponse =
            serde_json::from_str("{\"session_id\":\"s\",\"known_chunks\":3}").unwrap();
        assert_eq!(resp.known_chunks, 3);
        assert!(resp.delivery_mode.is_none());
        assert!(resp.bootstrap_size.is_none());
        assert!(resp.bootstrap_blake3.is_none());
        // And a v2 response omits absent fields on the wire.
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("delivery_mode"));
    }

    #[test]
    fn rejects_garbage() {
        assert!(BatchResponse::decode(b"nope").is_err());
        assert!(BatchResponse::decode(b"CVSP\x01\x01").is_err());
        // Trailing bytes are rejected too.
        let mut ok = BatchResponse::default().encode();
        ok.push(0);
        assert!(BatchResponse::decode(&ok).is_err());
    }
}
