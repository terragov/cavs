//! Content-addressed structural objects for CAVS.
//!
//! CAVS-1 stores chunks of payload. This crate generalises that into an
//! object graph: alongside chunks, a store can hold small immutable objects
//! — trees, commits, indexes, whatever a consumer defines — each naming the
//! objects it depends on.
//!
//! The split of responsibility is deliberate. CAVS knows ids, dependencies,
//! reachability and transfer. It does not know what a commit means; that
//! belongs to the layer above, which stores its schema in the opaque body of
//! an object.

pub mod bundle;
pub mod envelope;
pub mod error;
pub mod id;
pub mod store;
mod varint;
pub mod walk;

pub use bundle::{
    create_bundle, import_bundle, inspect_bundle, verify_bundle, BundleInfo, BundleLimits,
    BundleOptions, BundleRef, BundleSummary, BundleVerification, ImportReport, Signature,
    BUNDLE_FORMAT_V1, BUNDLE_MAGIC,
};
pub use envelope::{DecodeLimits, ObjectEnvelope, ENVELOPE_FORMAT_V1};
pub use error::{ObjectError, Result};
pub use id::{HashAlgorithm, ObjectId, ObjectKind, OBJECT_DOMAIN};
pub use store::{
    Durability, FsObjectStore, ObjectStore, StoreVerifyReport, StoredObject, VerifyResult,
    STORE_FORMAT_V1,
};
pub use walk::{
    compute_missing, walk_reachable, GraphSource, HaveNothing, HaveSet, KindFilter, MissingPlan,
    ObjectNode, Visit, Walk, WalkOptions, WalkState,
};
