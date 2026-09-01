//! Object identity: what class an object belongs to, and the hash that names
//! it.
//!
//! Identity is domain-separated by class. Two objects whose bytes are equal
//! but whose class differs get different ids, so a decoder can never be
//! tricked into reading a tree as a commit by feeding it a hash that happens
//! to match.

use core::fmt;

use cavs_hash::{from_hex, to_hex};

/// Domain tag mixed into every structural object id.
pub const OBJECT_DOMAIN: &[u8] = b"cavs-object-v1";

/// The class of a content-addressed object.
///
/// `Chunk` is the payload class CAVS already stores; every other variant is a
/// structural object introduced by the repository graph. `Application` lets a
/// consumer such as CAVS DB store an object class CAVS itself does not need to
/// understand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObjectKind {
    Chunk,
    Manifest,
    Tree,
    Commit,
    PackIndex,
    BundleIndex,
    Application(u32),
}

/// Application kinds live above this line so they can never collide with a
/// class CAVS defines later.
const APPLICATION_TAG_BASE: u32 = 0x8000_0000;

impl ObjectKind {
    /// The wire tag for this class. Stable: it is hashed into every object id.
    pub const fn tag(self) -> u32 {
        match self {
            ObjectKind::Chunk => 0,
            ObjectKind::Manifest => 1,
            ObjectKind::Tree => 2,
            ObjectKind::Commit => 3,
            ObjectKind::PackIndex => 4,
            ObjectKind::BundleIndex => 5,
            ObjectKind::Application(id) => APPLICATION_TAG_BASE | (id & !APPLICATION_TAG_BASE),
        }
    }

    /// Inverse of [`ObjectKind::tag`]. Unknown tags below the application
    /// range are rejected rather than guessed: they belong to a format
    /// version this build does not implement.
    pub const fn from_tag(tag: u32) -> Option<Self> {
        match tag {
            0 => Some(ObjectKind::Chunk),
            1 => Some(ObjectKind::Manifest),
            2 => Some(ObjectKind::Tree),
            3 => Some(ObjectKind::Commit),
            4 => Some(ObjectKind::PackIndex),
            5 => Some(ObjectKind::BundleIndex),
            t if t >= APPLICATION_TAG_BASE => {
                Some(ObjectKind::Application(t & !APPLICATION_TAG_BASE))
            }
            _ => None,
        }
    }

    /// Short lowercase name, used by the CLI and by error messages.
    pub fn name(self) -> &'static str {
        match self {
            ObjectKind::Chunk => "chunk",
            ObjectKind::Manifest => "manifest",
            ObjectKind::Tree => "tree",
            ObjectKind::Commit => "commit",
            ObjectKind::PackIndex => "packindex",
            ObjectKind::BundleIndex => "bundleindex",
            ObjectKind::Application(_) => "application",
        }
    }

    /// Chunks are payload: they carry bytes and never reference other
    /// objects, and their identity predates the object graph.
    pub const fn is_payload(self) -> bool {
        matches!(self, ObjectKind::Chunk)
    }
}

impl fmt::Display for ObjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ObjectKind::Application(id) => write!(f, "application({id})"),
            other => f.write_str(other.name()),
        }
    }
}

/// Hash function naming an object. Only BLAKE3-256 exists today; the field is
/// carried so a second algorithm can be added without reinterpreting stored
/// ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(u8)]
pub enum HashAlgorithm {
    #[default]
    Blake3 = 1,
}

impl HashAlgorithm {
    pub const fn code(self) -> u8 {
        self as u8
    }

    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(HashAlgorithm::Blake3),
            _ => None,
        }
    }
}

/// The name of an object: a hash algorithm plus its 256-bit digest.
///
/// Ordering is `(algorithm, digest)` lexicographically, which is the order
/// dependency lists are canonicalised into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId {
    pub algorithm: HashAlgorithm,
    pub digest: [u8; 32],
}

impl ObjectId {
    pub const fn new(algorithm: HashAlgorithm, digest: [u8; 32]) -> Self {
        ObjectId { algorithm, digest }
    }

    /// An id over a BLAKE3 digest that was computed elsewhere — a chunk hash
    /// from the existing store, for instance.
    pub const fn from_blake3(digest: [u8; 32]) -> Self {
        ObjectId {
            algorithm: HashAlgorithm::Blake3,
            digest,
        }
    }

    /// Compute the id of an object of class `kind` whose canonical bytes are
    /// `canonical`.
    ///
    /// Chunks are the one exception to domain separation: their identity is
    /// `blake3(payload)`, unchanged from CAVS-1, so that every manifest and
    /// packfile written before the object graph existed still names the same
    /// bytes. Every structural class is hashed as
    /// `blake3(domain || kind_tag || canonical)`.
    pub fn compute(kind: ObjectKind, canonical: &[u8]) -> Self {
        if kind.is_payload() {
            return ObjectId::from_blake3(cavs_hash::hash_chunk(canonical));
        }
        let mut hasher = blake3_hasher();
        hasher.update(OBJECT_DOMAIN);
        hasher.update(&kind.tag().to_le_bytes());
        hasher.update(canonical);
        ObjectId::from_blake3(hasher.finalize())
    }

    pub fn to_hex(self) -> String {
        to_hex(&self.digest)
    }

    /// Parse a bare 64-character hex digest as a BLAKE3 id.
    pub fn parse_hex(s: &str) -> Option<Self> {
        from_hex(s).map(ObjectId::from_blake3)
    }

    /// Does this id's hex form start with `prefix`? Used to resolve the
    /// abbreviated ids people type.
    pub fn has_hex_prefix(self, prefix: &str) -> bool {
        self.to_hex().starts_with(&prefix.to_ascii_lowercase())
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

fn blake3_hasher() -> cavs_hash::Hasher {
    cavs_hash::Hasher::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_tags_round_trip() {
        let kinds = [
            ObjectKind::Chunk,
            ObjectKind::Manifest,
            ObjectKind::Tree,
            ObjectKind::Commit,
            ObjectKind::PackIndex,
            ObjectKind::BundleIndex,
            ObjectKind::Application(0),
            ObjectKind::Application(7),
            ObjectKind::Application(0x7fff_ffff),
        ];
        for k in kinds {
            assert_eq!(ObjectKind::from_tag(k.tag()), Some(k), "{k}");
        }
    }

    #[test]
    fn unknown_structural_tag_is_rejected() {
        assert_eq!(ObjectKind::from_tag(6), None);
        assert_eq!(ObjectKind::from_tag(0x7fff_ffff), None);
    }

    #[test]
    fn identical_bytes_give_identical_ids() {
        let a = ObjectId::compute(ObjectKind::Tree, b"payload");
        let b = ObjectId::compute(ObjectKind::Tree, b"payload");
        assert_eq!(a, b);
    }

    #[test]
    fn one_byte_changes_the_id() {
        let a = ObjectId::compute(ObjectKind::Tree, b"payload");
        let b = ObjectId::compute(ObjectKind::Tree, b"payloae");
        assert_ne!(a, b);
    }

    #[test]
    fn changing_only_the_kind_changes_the_id() {
        let tree = ObjectId::compute(ObjectKind::Tree, b"same bytes");
        let commit = ObjectId::compute(ObjectKind::Commit, b"same bytes");
        let app = ObjectId::compute(ObjectKind::Application(2), b"same bytes");
        assert_ne!(tree, commit);
        assert_ne!(tree, app);
        assert_ne!(commit, app);
    }

    /// Chunk identity is the pre-existing CAVS-1 chunk hash, so the object
    /// graph can reference chunks already in a store without rewriting them.
    #[test]
    fn chunk_identity_matches_cavs1() {
        let payload = b"an existing chunk";
        assert_eq!(
            ObjectId::compute(ObjectKind::Chunk, payload).digest,
            cavs_hash::hash_chunk(payload)
        );
    }

    #[test]
    fn structural_identity_is_not_a_bare_hash() {
        let payload = b"an existing chunk";
        assert_ne!(
            ObjectId::compute(ObjectKind::Tree, payload).digest,
            cavs_hash::hash_chunk(payload)
        );
    }

    #[test]
    fn hex_round_trip() {
        let id = ObjectId::compute(ObjectKind::Commit, b"x");
        assert_eq!(ObjectId::parse_hex(&id.to_hex()), Some(id));
        assert!(id.has_hex_prefix(&id.to_hex()[..8]));
    }

    /// Interop anchor. These digests are part of the v1 format: if one of
    /// them changes, so has object identity, and the format version must be
    /// bumped rather than the vector edited.
    #[test]
    fn golden_object_ids() {
        assert_eq!(
            ObjectId::compute(ObjectKind::Tree, b"cavs-object-vector").to_hex(),
            "05c67866ac988a3c39d31a249e373b88e1048bc51bcf20870237d054878db606"
        );
        assert_eq!(
            ObjectId::compute(ObjectKind::Commit, b"cavs-object-vector").to_hex(),
            "304a7a604b43f30a9c112c098ee025d60633413f8a113a5f722f01f2f7532435"
        );
        assert_eq!(
            ObjectId::compute(ObjectKind::Chunk, b"cavs-object-vector").to_hex(),
            cavs_hash::to_hex(&cavs_hash::hash_chunk(b"cavs-object-vector"))
        );
    }
}
