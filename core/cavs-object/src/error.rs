//! Errors the object layer can report.

use crate::id::ObjectKind;

pub type Result<T> = std::result::Result<T, ObjectError>;

#[derive(Debug, thiserror::Error)]
pub enum ObjectError {
    #[error("truncated object: ran out of bytes reading {0}")]
    Truncated(&'static str),

    #[error("varint is not in canonical form")]
    VarintNotCanonical,

    #[error("unsupported object format version {0}")]
    UnsupportedFormat(u16),

    #[error("unknown object kind tag {0:#x}")]
    UnknownKind(u32),

    #[error("unknown hash algorithm {0}")]
    UnknownHashAlgorithm(u8),

    #[error("object declares kind {found} but was read as {expected}")]
    KindMismatch {
        expected: ObjectKind,
        found: ObjectKind,
    },

    #[error("dependencies are not in canonical order, or repeat")]
    DependenciesNotCanonical,

    #[error("a payload object cannot declare dependencies")]
    PayloadWithDependencies,

    #[error("{what} is {len} bytes, over the {max}-byte limit")]
    TooLarge {
        what: &'static str,
        len: usize,
        max: usize,
    },

    #[error("object declares {count} dependencies, over the limit of {max}")]
    TooManyDependencies { count: usize, max: usize },

    #[error("bytes remain after the object body")]
    TrailingBytes,

    #[error("object id mismatch: expected {expected}, computed {actual}")]
    IdMismatch { expected: String, actual: String },
}
