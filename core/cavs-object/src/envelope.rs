//! The canonical wire form of a structural object.
//!
//! An envelope is `(format_version, kind, dependencies, body)`. CAVS reads the
//! dependency list — that is what makes reachability, packing and transfer
//! possible without understanding a single byte of the body — and treats the
//! body as opaque.
//!
//! The encoding is canonical: every envelope has exactly one valid byte form,
//! and the decoder rejects any other. That is what lets two machines that
//! never spoke agree on an object id.
//!
//! ```text
//! format_version : u16 little-endian
//! kind           : u32 little-endian tag
//! dep_count      : LEB128 varuint
//! dependencies   : dep_count × (algorithm u8 ‖ digest[32]), strictly ascending
//! body_len       : LEB128 varuint
//! body           : body_len bytes
//! ```
//!
//! A chunk has no envelope. Its canonical bytes are its payload and its
//! dependency list is always empty, so the payload CAVS-1 already stores keeps
//! both its bytes and its identity.

use crate::error::{ObjectError, Result};
use crate::id::{HashAlgorithm, ObjectId, ObjectKind};
use crate::varint::{read_varuint, write_varuint};

/// Envelope format understood and written by this build.
pub const ENVELOPE_FORMAT_V1: u16 = 1;

/// Ceilings applied before a single byte is allocated, so a hostile object
/// cannot turn a length field into an out-of-memory abort.
///
/// Structural objects and payload are bounded separately. A tree page or a
/// commit is kilobytes and a fan-out of a million is already absurd; a chunk
/// is legitimately megabytes and has no fan-out at all. One number for both
/// would have to be the larger, which is no limit on the class that needs one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeLimits {
    /// Largest accepted structural object, envelope included.
    pub max_encoded_len: usize,
    /// Largest accepted structural body.
    pub max_body_len: usize,
    /// Largest accepted dependency count.
    pub max_dependencies: usize,
    /// Largest accepted payload object.
    pub max_payload_len: usize,
}

impl DecodeLimits {
    pub const DEFAULT: DecodeLimits = DecodeLimits {
        max_encoded_len: 64 * 1024 * 1024,
        max_body_len: 64 * 1024 * 1024,
        max_dependencies: 1 << 20,
        max_payload_len: 1 << 30,
    };

    pub const fn with_max_payload_len(mut self, max: usize) -> Self {
        self.max_payload_len = max;
        self
    }

    /// The ceiling that applies to an object of this class.
    pub const fn max_len_for(&self, kind: ObjectKind) -> usize {
        if kind.is_payload() {
            self.max_payload_len
        } else {
            self.max_encoded_len
        }
    }
}

impl Default for DecodeLimits {
    fn default() -> Self {
        DecodeLimits::DEFAULT
    }
}

/// A structural object, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectEnvelope {
    pub format_version: u16,
    pub kind: ObjectKind,
    /// Objects this one references, canonically ordered and deduplicated.
    pub dependencies: Vec<ObjectId>,
    pub body: Vec<u8>,
}

impl ObjectEnvelope {
    /// Build an envelope, canonicalising the dependency list.
    ///
    /// Dependencies are sorted and deduplicated here rather than being
    /// required in order from the caller: order carries no meaning for
    /// reachability, so leaving it to the caller would only create two byte
    /// forms of the same object.
    pub fn new(kind: ObjectKind, dependencies: Vec<ObjectId>, body: Vec<u8>) -> Result<Self> {
        let mut dependencies = dependencies;
        dependencies.sort_unstable();
        dependencies.dedup();
        if kind.is_payload() && !dependencies.is_empty() {
            return Err(ObjectError::PayloadWithDependencies);
        }
        Ok(ObjectEnvelope {
            format_version: ENVELOPE_FORMAT_V1,
            kind,
            dependencies,
            body,
        })
    }

    /// A leaf object: bytes with no references.
    pub fn leaf(kind: ObjectKind, body: Vec<u8>) -> Result<Self> {
        ObjectEnvelope::new(kind, Vec::new(), body)
    }

    /// The bytes this object is named by and stored as.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        if self.kind.is_payload() {
            return self.body.clone();
        }
        let mut out = Vec::with_capacity(self.encoded_len_hint());
        out.extend_from_slice(&self.format_version.to_le_bytes());
        out.extend_from_slice(&self.kind.tag().to_le_bytes());
        write_varuint(self.dependencies.len() as u64, &mut out);
        for dep in &self.dependencies {
            out.push(dep.algorithm.code());
            out.extend_from_slice(&dep.digest);
        }
        write_varuint(self.body.len() as u64, &mut out);
        out.extend_from_slice(&self.body);
        out
    }

    fn encoded_len_hint(&self) -> usize {
        2 + 4 + 10 + self.dependencies.len() * 33 + 10 + self.body.len()
    }

    /// This object's id.
    pub fn id(&self) -> ObjectId {
        ObjectId::compute(self.kind, &self.canonical_bytes())
    }

    /// Decode `bytes` as an object of class `kind`.
    ///
    /// The class comes from the store rather than from the bytes because it is
    /// hashed into the id: an object read under the wrong class simply fails
    /// its id check.
    pub fn decode(kind: ObjectKind, bytes: &[u8], limits: DecodeLimits) -> Result<Self> {
        let max = limits.max_len_for(kind);
        if bytes.len() > max {
            return Err(ObjectError::TooLarge {
                what: "object",
                len: bytes.len(),
                max,
            });
        }
        if kind.is_payload() {
            return Ok(ObjectEnvelope {
                format_version: ENVELOPE_FORMAT_V1,
                kind,
                dependencies: Vec::new(),
                body: bytes.to_vec(),
            });
        }

        let mut input = bytes;
        let format_version = u16::from_le_bytes(take_array::<2>(&mut input, "format_version")?);
        if format_version != ENVELOPE_FORMAT_V1 {
            return Err(ObjectError::UnsupportedFormat(format_version));
        }
        let tag = u32::from_le_bytes(take_array::<4>(&mut input, "kind")?);
        let decoded_kind = ObjectKind::from_tag(tag).ok_or(ObjectError::UnknownKind(tag))?;
        if decoded_kind != kind {
            return Err(ObjectError::KindMismatch {
                expected: kind,
                found: decoded_kind,
            });
        }

        let dep_count = read_varuint(&mut input)? as usize;
        if dep_count > limits.max_dependencies {
            return Err(ObjectError::TooManyDependencies {
                count: dep_count,
                max: limits.max_dependencies,
            });
        }
        // A dependency is 33 bytes; refusing to reserve for more than the
        // input can possibly hold keeps a forged count from allocating.
        if dep_count.saturating_mul(33) > input.len() {
            return Err(ObjectError::Truncated("dependencies"));
        }
        let mut dependencies = Vec::with_capacity(dep_count);
        let mut previous: Option<ObjectId> = None;
        for _ in 0..dep_count {
            let algorithm_code = take_array::<1>(&mut input, "dependency algorithm")?[0];
            let algorithm = HashAlgorithm::from_code(algorithm_code)
                .ok_or(ObjectError::UnknownHashAlgorithm(algorithm_code))?;
            let digest = take_array::<32>(&mut input, "dependency digest")?;
            let id = ObjectId::new(algorithm, digest);
            if let Some(prev) = previous {
                if id <= prev {
                    return Err(ObjectError::DependenciesNotCanonical);
                }
            }
            previous = Some(id);
            dependencies.push(id);
        }

        let body_len = read_varuint(&mut input)? as usize;
        if body_len > limits.max_body_len {
            return Err(ObjectError::TooLarge {
                what: "body",
                len: body_len,
                max: limits.max_body_len,
            });
        }
        if body_len != input.len() {
            return Err(ObjectError::TrailingBytes);
        }
        let body = input.to_vec();

        Ok(ObjectEnvelope {
            format_version,
            kind,
            dependencies,
            body,
        })
    }

    /// Decode and check that the bytes really are the object `expected` names.
    pub fn decode_verified(
        kind: ObjectKind,
        expected: ObjectId,
        bytes: &[u8],
        limits: DecodeLimits,
    ) -> Result<Self> {
        let actual = ObjectId::compute(kind, bytes);
        if actual != expected {
            return Err(ObjectError::IdMismatch {
                expected: expected.to_hex(),
                actual: actual.to_hex(),
            });
        }
        ObjectEnvelope::decode(kind, bytes, limits)
    }
}

fn take_array<const N: usize>(input: &mut &[u8], what: &'static str) -> Result<[u8; N]> {
    if input.len() < N {
        return Err(ObjectError::Truncated(what));
    }
    let (head, rest) = input.split_at(N);
    *input = rest;
    let mut out = [0u8; N];
    out.copy_from_slice(head);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dep(n: u8) -> ObjectId {
        ObjectId::from_blake3([n; 32])
    }

    #[test]
    fn round_trip_without_dependencies() {
        let env = ObjectEnvelope::leaf(ObjectKind::Tree, b"leaf body".to_vec()).unwrap();
        let bytes = env.canonical_bytes();
        let back = ObjectEnvelope::decode(ObjectKind::Tree, &bytes, DecodeLimits::DEFAULT).unwrap();
        assert_eq!(back, env);
    }

    #[test]
    fn round_trip_with_dependencies() {
        let env = ObjectEnvelope::new(
            ObjectKind::Commit,
            vec![dep(9), dep(1), dep(4)],
            b"commit body".to_vec(),
        )
        .unwrap();
        assert_eq!(env.dependencies, vec![dep(1), dep(4), dep(9)]);
        let bytes = env.canonical_bytes();
        let back =
            ObjectEnvelope::decode(ObjectKind::Commit, &bytes, DecodeLimits::DEFAULT).unwrap();
        assert_eq!(back, env);
    }

    /// Dependency order carries no meaning, so any permutation of the same
    /// set has to produce the same id.
    #[test]
    fn dependency_order_does_not_change_the_id() {
        let a =
            ObjectEnvelope::new(ObjectKind::Tree, vec![dep(3), dep(1), dep(2)], vec![]).unwrap();
        let b =
            ObjectEnvelope::new(ObjectKind::Tree, vec![dep(2), dep(3), dep(1)], vec![]).unwrap();
        assert_eq!(a.id(), b.id());
    }

    #[test]
    fn duplicate_dependencies_collapse() {
        let env =
            ObjectEnvelope::new(ObjectKind::Tree, vec![dep(1), dep(1), dep(1)], vec![]).unwrap();
        assert_eq!(env.dependencies, vec![dep(1)]);
    }

    #[test]
    fn chunk_bytes_are_the_payload_verbatim() {
        let env = ObjectEnvelope::leaf(ObjectKind::Chunk, b"raw payload".to_vec()).unwrap();
        assert_eq!(env.canonical_bytes(), b"raw payload");
        assert_eq!(env.id().digest, cavs_hash::hash_chunk(b"raw payload"));
    }

    #[test]
    fn a_chunk_cannot_declare_dependencies() {
        assert!(matches!(
            ObjectEnvelope::new(ObjectKind::Chunk, vec![dep(1)], vec![]),
            Err(ObjectError::PayloadWithDependencies)
        ));
    }

    #[test]
    fn decoding_under_the_wrong_kind_fails() {
        let env = ObjectEnvelope::leaf(ObjectKind::Tree, b"x".to_vec()).unwrap();
        let bytes = env.canonical_bytes();
        assert!(matches!(
            ObjectEnvelope::decode(ObjectKind::Commit, &bytes, DecodeLimits::DEFAULT),
            Err(ObjectError::KindMismatch { .. })
        ));
    }

    #[test]
    fn out_of_order_dependencies_are_rejected() {
        let env = ObjectEnvelope::new(ObjectKind::Tree, vec![dep(1), dep(2)], vec![]).unwrap();
        let mut bytes = env.canonical_bytes();
        // Swap the two 33-byte dependency records that follow the 7-byte head.
        let head = 2 + 4 + 1;
        let (first, second) = (head, head + 33);
        for i in 0..33 {
            bytes.swap(first + i, second + i);
        }
        assert!(matches!(
            ObjectEnvelope::decode(ObjectKind::Tree, &bytes, DecodeLimits::DEFAULT),
            Err(ObjectError::DependenciesNotCanonical)
        ));
    }

    #[test]
    fn duplicate_dependencies_on_the_wire_are_rejected() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&ENVELOPE_FORMAT_V1.to_le_bytes());
        bytes.extend_from_slice(&ObjectKind::Tree.tag().to_le_bytes());
        write_varuint(2, &mut bytes);
        for _ in 0..2 {
            bytes.push(HashAlgorithm::Blake3.code());
            bytes.extend_from_slice(&[7u8; 32]);
        }
        write_varuint(0, &mut bytes);
        assert!(matches!(
            ObjectEnvelope::decode(ObjectKind::Tree, &bytes, DecodeLimits::DEFAULT),
            Err(ObjectError::DependenciesNotCanonical)
        ));
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let env = ObjectEnvelope::leaf(ObjectKind::Tree, b"body".to_vec()).unwrap();
        let mut bytes = env.canonical_bytes();
        bytes.push(0);
        assert!(matches!(
            ObjectEnvelope::decode(ObjectKind::Tree, &bytes, DecodeLimits::DEFAULT),
            Err(ObjectError::TrailingBytes)
        ));
    }

    #[test]
    fn truncation_is_rejected() {
        let env = ObjectEnvelope::new(ObjectKind::Tree, vec![dep(1)], b"body".to_vec()).unwrap();
        let bytes = env.canonical_bytes();
        for cut in 0..bytes.len() {
            assert!(
                ObjectEnvelope::decode(ObjectKind::Tree, &bytes[..cut], DecodeLimits::DEFAULT)
                    .is_err(),
                "truncation at {cut} decoded"
            );
        }
    }

    /// A forged dependency count must not make the decoder reserve for
    /// dependencies the input could not possibly contain.
    #[test]
    fn a_lying_dependency_count_does_not_allocate() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&ENVELOPE_FORMAT_V1.to_le_bytes());
        bytes.extend_from_slice(&ObjectKind::Tree.tag().to_le_bytes());
        write_varuint(1_000_000, &mut bytes);
        assert!(matches!(
            ObjectEnvelope::decode(ObjectKind::Tree, &bytes, DecodeLimits::DEFAULT),
            Err(ObjectError::Truncated("dependencies"))
        ));
    }

    #[test]
    fn limits_are_enforced() {
        let env = ObjectEnvelope::leaf(ObjectKind::Tree, vec![0u8; 4096]).unwrap();
        let bytes = env.canonical_bytes();
        let tight = DecodeLimits {
            max_encoded_len: 64,
            ..DecodeLimits::DEFAULT
        };
        assert!(matches!(
            ObjectEnvelope::decode(ObjectKind::Tree, &bytes, tight),
            Err(ObjectError::TooLarge { .. })
        ));
    }

    #[test]
    fn verified_decode_rejects_a_wrong_id() {
        let env = ObjectEnvelope::leaf(ObjectKind::Tree, b"body".to_vec()).unwrap();
        let bytes = env.canonical_bytes();
        assert!(ObjectEnvelope::decode_verified(
            ObjectKind::Tree,
            env.id(),
            &bytes,
            DecodeLimits::DEFAULT
        )
        .is_ok());
        assert!(matches!(
            ObjectEnvelope::decode_verified(
                ObjectKind::Tree,
                ObjectId::from_blake3([0; 32]),
                &bytes,
                DecodeLimits::DEFAULT
            ),
            Err(ObjectError::IdMismatch { .. })
        ));
    }

    /// Golden bytes for the v1 envelope. Any change here is a format change.
    #[test]
    fn golden_encoding() {
        let env = ObjectEnvelope::new(
            ObjectKind::Commit,
            vec![ObjectId::from_blake3([0xaa; 32])],
            b"hi".to_vec(),
        )
        .unwrap();
        let bytes = env.canonical_bytes();
        let mut want = Vec::new();
        want.extend_from_slice(&[0x01, 0x00]); // format_version = 1
        want.extend_from_slice(&[0x03, 0x00, 0x00, 0x00]); // kind = Commit
        want.push(0x01); // one dependency
        want.push(0x01); // BLAKE3
        want.extend_from_slice(&[0xaa; 32]);
        want.push(0x02); // body length
        want.extend_from_slice(b"hi");
        assert_eq!(bytes, want);
        assert_eq!(
            env.id().to_hex(),
            "d23a1ea11c048b2cfe8637d094718ecfc58aa0c9411e175751862355745ca06b"
        );
    }
}
