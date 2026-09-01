//! A subgraph in a file.
//!
//! A bundle is what a repository state looks like when there is no server: a
//! single file holding a set of objects, the roots they hang from, and enough
//! structure to prove on arrival that it is all there and none of it changed
//! on the way. USB stick, CI artifact, air gap, static bucket — the transport
//! does not have to be trusted, because the file checks out or it does not.
//!
//! # Nothing is published until all of it verifies
//!
//! Importing reads the footer, then the table, then every object, checking
//! each against the id it is filed under, and only then writes anything into
//! the store. A truncated or edited bundle fails while the store is still
//! exactly as it was. Importing the same bundle twice leaves the store the
//! same as importing it once, because an object's name is its hash.
//!
//! # Layout
//!
//! ```text
//! magic            "CAVSBND1"
//! format_version   u16
//! flags            u16          bit 0: payload is zstd
//! created_at       u64          unix seconds
//! roots            count, then (kind, id) each
//! prerequisites    count, then id each — objects a thin bundle assumes
//! refs             count, then (name, id) each — opaque to CAVS
//! object table     count, then (kind, id, offset, length) each
//! payload_len      u64          bytes as stored
//! payload_raw_len  u64          bytes once decompressed
//! payload          the objects, back to back
//! signatures       count, then (key id, 64-byte signature) each
//! footer           content_len u64
//!                  content checksum, blake3 over everything before the
//!                                    signature block — this is what a
//!                                    signature signs
//!                  file checksum, blake3 over everything before itself
//!                  the magic again
//! ```
//!
//! The trailing magic is what makes truncation loud: a file that stops early
//! does not end where a bundle ends.
//!
//! There are two checksums because a signature cannot cover itself. The
//! content checksum stops at the signature block, so signing a bundle does not
//! change what its signatures are over — which is what lets a second signer add
//! theirs to a bundle the first one already signed. The file checksum covers
//! everything including the signatures, so nothing in the file can be edited
//! unnoticed.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::Path;

use crate::envelope::DecodeLimits;
use crate::error::{ObjectError, Result};
use crate::id::{HashAlgorithm, ObjectId, ObjectKind};
use crate::store::{ObjectStore, StoredObject};
use crate::varint::{read_varuint, write_varuint};
use crate::walk::{walk_reachable, GraphSource, HaveSet, WalkOptions};

pub const BUNDLE_MAGIC: &[u8; 8] = b"CAVSBND1";
pub const BUNDLE_FORMAT_V1: u16 = 1;

/// Payload is zstd-compressed.
pub const FLAG_ZSTD: u16 = 1 << 0;

/// Ceilings a bundle is read under, applied before anything is allocated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BundleLimits {
    pub max_objects: usize,
    pub max_roots: usize,
    pub max_refs: usize,
    /// Largest payload this reader will decompress into memory. The one that
    /// matters: a small file claiming a huge decompressed size is the classic
    /// way to turn a download into an out-of-memory kill.
    pub max_payload_bytes: u64,
    /// Largest ratio of decompressed to stored bytes that will be accepted.
    pub max_expansion_ratio: u64,
    pub objects: DecodeLimits,
}

impl Default for BundleLimits {
    fn default() -> Self {
        BundleLimits {
            max_objects: 100_000_000,
            max_roots: 4096,
            max_refs: 65_536,
            max_payload_bytes: 64 << 30,
            max_expansion_ratio: 1000,
            objects: DecodeLimits::DEFAULT,
        }
    }
}

/// A name the producer attached to a root. CAVS does not interpret it — a
/// branch means nothing here — but it carries it so the importer can offer a
/// choice of what to point at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleRef {
    pub name: String,
    pub target: ObjectId,
}

/// What a bundle says about itself, without reading its payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleInfo {
    pub format_version: u16,
    pub compressed: bool,
    pub created_at: u64,
    pub roots: Vec<(ObjectKind, ObjectId)>,
    /// Objects this bundle references but does not contain. A bundle with any
    /// of these is thin: it only applies to a store that already has them.
    pub prerequisites: Vec<ObjectId>,
    pub refs: Vec<BundleRef>,
    pub object_count: u64,
    pub payload_bytes: u64,
    pub payload_raw_bytes: u64,
    pub signatures: Vec<Signature>,
}

impl BundleInfo {
    pub fn is_thin(&self) -> bool {
        !self.prerequisites.is_empty()
    }
}

/// A detached signature over the bundle's contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    /// Which key made it, so a verifier can find the right public key without
    /// trying all of them.
    pub key_id: [u8; 8],
    pub bytes: [u8; 64],
}

/// What to put in a bundle.
#[derive(Debug, Clone, Default)]
pub struct BundleOptions {
    pub refs: Vec<BundleRef>,
    /// Leave out anything the receiver already has, and record it as a
    /// prerequisite instead.
    pub exclude_have: Option<BTreeSet<ObjectId>>,
    /// Walk restrictions — metadata-only, for instance.
    pub walk: Option<WalkOptions>,
    pub compress: bool,
    pub created_at: Option<u64>,
}

impl BundleOptions {
    pub fn with_refs(mut self, refs: Vec<BundleRef>) -> Self {
        self.refs = refs;
        self
    }

    pub fn excluding(mut self, have: BTreeSet<ObjectId>) -> Self {
        self.exclude_have = Some(have);
        self
    }

    pub fn compressed(mut self) -> Self {
        self.compress = true;
        self
    }
}

/// What went into a bundle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BundleSummary {
    pub objects: u64,
    pub payload_bytes: u64,
    pub file_bytes: u64,
    pub prerequisites: u64,
}

/// Write the subgraph under `roots` to `out`.
pub fn create_bundle<S>(
    store: &S,
    roots: &[(ObjectKind, ObjectId)],
    out: &Path,
    options: &BundleOptions,
) -> Result<BundleSummary>
where
    S: ObjectStore + GraphSource + ?Sized,
{
    let walk_options = options.walk.clone().unwrap_or_default();
    let root_ids: Vec<ObjectId> = roots.iter().map(|(_, id)| *id).collect();

    // Which objects travel. With a have-set, the plan prunes whole subtrees
    // the receiver already holds rather than walking into them: claiming an
    // object claims its closure.
    let included: Vec<ObjectId> = match &options.exclude_have {
        Some(have) => {
            let plan = crate::walk::compute_missing(
                store,
                &root_ids,
                have,
                walk_options.clone().requiring_every_object(),
            )?;
            plan.missing
        }
        None => walk_reachable(store, &root_ids, walk_options.requiring_every_object())?
            .into_iter()
            .map(|visit| visit.id)
            .collect(),
    };
    let included_set: BTreeSet<ObjectId> = included.iter().copied().collect();

    let mut payload: Vec<u8> = Vec::new();
    let mut table: Vec<(ObjectKind, ObjectId, u64, u64)> = Vec::new();
    let mut prerequisites: BTreeSet<ObjectId> = BTreeSet::new();

    for id in &included {
        let object = store.get_object(id)?;
        // A prerequisite is something the receiver is expected to already
        // have — the frontier where this bundle stops because the have-set
        // said to. Only the frontier, not the whole excluded closure: the
        // receiver holding an object means holding everything under it, so
        // naming the closure would make a thin bundle grow with the size of
        // what it is *not* sending.
        //
        // An object left out by a kind filter is a different thing entirely.
        // A metadata-only bundle is not assuming the receiver has the payload;
        // it is deliberately incomplete, and calling that a prerequisite would
        // let it verify as whole when it is not.
        if let Some(have) = &options.exclude_have {
            for dependency in object.dependencies(limits_for(&object))? {
                if !included_set.contains(&dependency) && have.may_have(&dependency) {
                    prerequisites.insert(dependency);
                }
            }
        }
        let offset = payload.len() as u64;
        payload.extend_from_slice(&object.bytes);
        table.push((object.kind, *id, offset, object.bytes.len() as u64));
    }

    let prerequisites: Vec<ObjectId> = prerequisites.into_iter().collect();

    let raw_len = payload.len() as u64;
    let stored_payload = if options.compress {
        zstd_compress(&payload)?
    } else {
        payload
    };

    let mut body = Vec::with_capacity(stored_payload.len() + table.len() * 48 + 128);
    body.extend_from_slice(BUNDLE_MAGIC);
    body.extend_from_slice(&BUNDLE_FORMAT_V1.to_le_bytes());
    let flags = if options.compress { FLAG_ZSTD } else { 0 };
    body.extend_from_slice(&flags.to_le_bytes());
    body.extend_from_slice(&options.created_at.unwrap_or(0).to_le_bytes());

    write_varuint(roots.len() as u64, &mut body);
    for (kind, id) in roots {
        body.extend_from_slice(&kind.tag().to_le_bytes());
        write_id(id, &mut body);
    }
    write_varuint(prerequisites.len() as u64, &mut body);
    for id in &prerequisites {
        write_id(id, &mut body);
    }
    write_varuint(options.refs.len() as u64, &mut body);
    for reference in &options.refs {
        write_varuint(reference.name.len() as u64, &mut body);
        body.extend_from_slice(reference.name.as_bytes());
        write_id(&reference.target, &mut body);
    }

    write_varuint(table.len() as u64, &mut body);
    for (kind, id, offset, length) in &table {
        body.extend_from_slice(&kind.tag().to_le_bytes());
        write_id(id, &mut body);
        write_varuint(*offset, &mut body);
        write_varuint(*length, &mut body);
    }

    body.extend_from_slice(&(stored_payload.len() as u64).to_le_bytes());
    body.extend_from_slice(&raw_len.to_le_bytes());
    body.extend_from_slice(&stored_payload);

    let content_len = body.len() as u64;
    let content_checksum = cavs_hash::hash_chunk(&body);

    // No signatures yet; a signer appends them and rewrites the footer,
    // leaving the content checksum — and so every existing signature — alone.
    write_varuint(0, &mut body);
    seal(&mut body, content_len, &content_checksum);

    write_atomically(out, &body)?;

    Ok(BundleSummary {
        objects: table.len() as u64,
        payload_bytes: raw_len,
        file_bytes: body.len() as u64,
        prerequisites: prerequisites.len() as u64,
    })
}

/// A bundle opened and structurally checked, but with its objects not yet
/// verified.
struct OpenBundle {
    info: BundleInfo,
    table: Vec<(ObjectKind, ObjectId, u64, u64)>,
    payload: Vec<u8>,
    content_checksum: [u8; 32],
}

/// content_len, content checksum, file checksum, magic.
const FOOTER_LEN: usize = 8 + 32 + 32 + 8;

/// Close a bundle: write the footer over whatever body it has now.
fn seal(body: &mut Vec<u8>, content_len: u64, content_checksum: &[u8; 32]) {
    body.extend_from_slice(&content_len.to_le_bytes());
    body.extend_from_slice(content_checksum);
    let file_checksum = cavs_hash::hash_chunk(body);
    body.extend_from_slice(&file_checksum);
    body.extend_from_slice(BUNDLE_MAGIC);
}

/// The bytes a signature over this bundle covers.
pub fn bundle_content_checksum(path: &Path, limits: &BundleLimits) -> Result<[u8; 32]> {
    Ok(open(path, limits)?.content_checksum)
}

/// Add a signature to a bundle that is already written.
///
/// The content checksum stops before the signature block, so adding one does
/// not disturb any signature already there: a bundle can collect signatures
/// from several publishers without any of them having to re-sign.
pub fn append_signature(path: &Path, signature: Signature, limits: &BundleLimits) -> Result<()> {
    let raw = fs::read(path).map_err(io_err("read bundle"))?;
    // Reading it back through the front door is the point: a bundle that no
    // longer verifies is not one to sign.
    let opened = open_bytes(&raw, limits)?;
    let footer_at = raw.len() - FOOTER_LEN;
    let content_len =
        u64::from_le_bytes(raw[footer_at..footer_at + 8].try_into().expect("8 bytes"));

    let mut body = raw[..content_len as usize].to_vec();
    let mut signatures = opened.info.signatures;
    if signatures.iter().any(|s| s.key_id == signature.key_id) {
        return Err(ObjectError::Corrupt(
            "this bundle already carries a signature from that key".into(),
        ));
    }
    signatures.push(signature);
    write_varuint(signatures.len() as u64, &mut body);
    for signature in &signatures {
        body.extend_from_slice(&signature.key_id);
        body.extend_from_slice(&signature.bytes);
    }
    seal(&mut body, content_len, &opened.content_checksum);
    write_atomically(path, &body)
}

fn open(path: &Path, limits: &BundleLimits) -> Result<OpenBundle> {
    let raw = fs::read(path).map_err(io_err("read bundle"))?;
    open_bytes(&raw, limits)
}

fn open_bytes(raw: &[u8], limits: &BundleLimits) -> Result<OpenBundle> {
    // The footer first. Everything else is only worth reading if the file
    // says it is whole, and the trailing magic is what a truncation removes.
    if raw.len() < BUNDLE_MAGIC.len() + FOOTER_LEN {
        return Err(ObjectError::Truncated("bundle"));
    }
    if &raw[..8] != BUNDLE_MAGIC {
        return Err(ObjectError::Corrupt(
            "this file does not start like a bundle".into(),
        ));
    }
    let (body, footer) = raw.split_at(raw.len() - FOOTER_LEN);
    if &footer[FOOTER_LEN - 8..] != BUNDLE_MAGIC {
        return Err(ObjectError::Corrupt(
            "bundle does not end where a bundle ends; it is truncated or damaged".into(),
        ));
    }
    // The file checksum covers everything before itself, signatures included.
    let signed_region = &raw[..raw.len() - 40];
    if cavs_hash::hash_chunk(signed_region) != footer[40..72] {
        return Err(ObjectError::Corrupt(
            "bundle checksum does not match its contents".into(),
        ));
    }
    let content_len = u64::from_le_bytes(footer[..8].try_into().expect("8 bytes"));
    let mut content_checksum = [0u8; 32];
    content_checksum.copy_from_slice(&footer[8..40]);
    if content_len as usize > body.len() {
        return Err(ObjectError::Corrupt(
            "bundle says its signed region runs past its own end".into(),
        ));
    }
    if cavs_hash::hash_chunk(&body[..content_len as usize]) != content_checksum {
        return Err(ObjectError::Corrupt(
            "bundle content checksum does not match what the signatures cover".into(),
        ));
    }

    let mut input = &body[8..];
    let format_version = u16::from_le_bytes(take::<2>(&mut input, "format version")?);
    if format_version != BUNDLE_FORMAT_V1 {
        return Err(ObjectError::UnsupportedFormat(format_version));
    }
    let flags = u16::from_le_bytes(take::<2>(&mut input, "flags")?);
    if flags & !FLAG_ZSTD != 0 {
        return Err(ObjectError::Corrupt(format!(
            "bundle sets flag bits {flags:#x} this build does not understand"
        )));
    }
    let created_at = u64::from_le_bytes(take::<8>(&mut input, "created_at")?);

    let root_count = bounded(read_varuint(&mut input)?, limits.max_roots, "roots")?;
    let mut roots = Vec::with_capacity(root_count);
    for _ in 0..root_count {
        let tag = u32::from_le_bytes(take::<4>(&mut input, "root kind")?);
        let kind = ObjectKind::from_tag(tag).ok_or(ObjectError::UnknownKind(tag))?;
        roots.push((kind, read_id(&mut input)?));
    }

    let prerequisite_count = bounded(
        read_varuint(&mut input)?,
        limits.max_objects,
        "prerequisites",
    )?;
    if prerequisite_count.saturating_mul(33) > input.len() {
        return Err(ObjectError::Truncated("prerequisites"));
    }
    let mut prerequisites = Vec::with_capacity(prerequisite_count);
    for _ in 0..prerequisite_count {
        prerequisites.push(read_id(&mut input)?);
    }

    let ref_count = bounded(read_varuint(&mut input)?, limits.max_refs, "refs")?;
    let mut refs = Vec::with_capacity(ref_count.min(1024));
    for _ in 0..ref_count {
        let name_len = read_varuint(&mut input)? as usize;
        if name_len > 512 || name_len > input.len() {
            return Err(ObjectError::Corrupt("bundle reference name is bad".into()));
        }
        let (name, rest) = input.split_at(name_len);
        input = rest;
        let name = String::from_utf8(name.to_vec())
            .map_err(|_| ObjectError::Corrupt("bundle reference name is not UTF-8".into()))?;
        refs.push(BundleRef {
            name,
            target: read_id(&mut input)?,
        });
    }

    let object_count = bounded(read_varuint(&mut input)?, limits.max_objects, "objects")?;
    // A table row is at least 4 + 33 + 1 + 1 bytes; a count larger than the
    // input could hold is a lie, and reserving for it is the bug.
    if object_count.saturating_mul(39) > input.len() {
        return Err(ObjectError::Corrupt(
            "bundle object count is larger than the bytes that follow".into(),
        ));
    }
    let mut table = Vec::with_capacity(object_count);
    for _ in 0..object_count {
        let tag = u32::from_le_bytes(take::<4>(&mut input, "object kind")?);
        let kind = ObjectKind::from_tag(tag).ok_or(ObjectError::UnknownKind(tag))?;
        let id = read_id(&mut input)?;
        let offset = read_varuint(&mut input)?;
        let length = read_varuint(&mut input)?;
        table.push((kind, id, offset, length));
    }

    let payload_len = u64::from_le_bytes(take::<8>(&mut input, "payload length")?);
    let payload_raw_len = u64::from_le_bytes(take::<8>(&mut input, "payload raw length")?);
    if payload_raw_len > limits.max_payload_bytes {
        return Err(ObjectError::TooLarge {
            what: "bundle payload",
            len: payload_raw_len as usize,
            max: limits.max_payload_bytes as usize,
        });
    }
    // A file that claims to expand a thousandfold is not a bundle anyone
    // meant to send.
    if flags & FLAG_ZSTD != 0
        && payload_raw_len
            > payload_len
                .max(1)
                .saturating_mul(limits.max_expansion_ratio)
    {
        return Err(ObjectError::Corrupt(format!(
            "bundle claims {payload_raw_len} bytes from {payload_len} stored, past the \
             {}x expansion this reader accepts",
            limits.max_expansion_ratio
        )));
    }
    if payload_len as usize > input.len() {
        return Err(ObjectError::Truncated("bundle payload"));
    }
    let (stored_payload, mut rest) = input.split_at(payload_len as usize);

    let payload = if flags & FLAG_ZSTD != 0 {
        zstd_decompress(stored_payload, payload_raw_len as usize)?
    } else {
        if payload_len != payload_raw_len {
            return Err(ObjectError::Corrupt(
                "an uncompressed bundle disagrees with itself about its payload size".into(),
            ));
        }
        stored_payload.to_vec()
    };

    let signature_count = bounded(read_varuint(&mut rest)?, 64, "signatures")?;
    let mut signatures = Vec::with_capacity(signature_count);
    for _ in 0..signature_count {
        let key_id = take::<8>(&mut rest, "signature key id")?;
        let bytes = take::<64>(&mut rest, "signature")?;
        signatures.push(Signature { key_id, bytes });
    }
    if !rest.is_empty() {
        return Err(ObjectError::TrailingBytes);
    }

    Ok(OpenBundle {
        content_checksum,
        info: BundleInfo {
            format_version,
            compressed: flags & FLAG_ZSTD != 0,
            created_at,
            roots,
            prerequisites,
            refs,
            object_count: table.len() as u64,
            payload_bytes: payload_len,
            payload_raw_bytes: payload_raw_len,
            signatures,
        },
        table,
        payload,
    })
}

/// Read a bundle's header without verifying its objects.
pub fn inspect_bundle(path: &Path, limits: &BundleLimits) -> Result<BundleInfo> {
    Ok(open(path, limits)?.info)
}

/// What a full verification found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleVerification {
    pub info: BundleInfo,
    pub objects_verified: u64,
    pub bytes_verified: u64,
    /// Roots the bundle advertises but does not contain and does not list as
    /// prerequisites: it cannot deliver what it promises.
    pub unsatisfied_roots: Vec<ObjectId>,
    /// Objects referenced from inside the bundle that are neither in it nor
    /// declared as prerequisites.
    pub dangling: Vec<ObjectId>,
}

impl BundleVerification {
    pub fn is_complete(&self) -> bool {
        self.unsatisfied_roots.is_empty() && self.dangling.is_empty()
    }
}

/// Check every object in a bundle against the id it is filed under, and check
/// that the graph inside it is closed.
pub fn verify_bundle(path: &Path, limits: &BundleLimits) -> Result<BundleVerification> {
    let bundle = open(path, limits)?;
    verify_open(&bundle, limits)
}

fn verify_open(bundle: &OpenBundle, limits: &BundleLimits) -> Result<BundleVerification> {
    let mut contents: BTreeMap<ObjectId, (ObjectKind, &[u8])> = BTreeMap::new();
    let mut bytes_verified = 0u64;

    for (kind, id, offset, length) in &bundle.table {
        let start = *offset as usize;
        let end = start
            .checked_add(*length as usize)
            .ok_or_else(|| ObjectError::Corrupt("bundle table entry overflows".into()))?;
        if end > bundle.payload.len() {
            return Err(ObjectError::Corrupt(format!(
                "bundle table points object {id} past the end of the payload"
            )));
        }
        let slice = &bundle.payload[start..end];
        let actual = ObjectId::compute(*kind, slice);
        if actual != *id {
            return Err(ObjectError::IdMismatch {
                expected: id.to_hex(),
                actual: actual.to_hex(),
            });
        }
        // Two entries under one id must be the same bytes, or the bundle is
        // trying to make one name mean two things.
        if let Some((existing_kind, existing)) = contents.get(id) {
            if existing_kind != kind || *existing != slice {
                return Err(ObjectError::Corrupt(format!(
                    "bundle contains two different objects under {id}"
                )));
            }
            continue;
        }
        contents.insert(*id, (*kind, slice));
        bytes_verified += *length;
    }

    let known: BTreeSet<ObjectId> = contents
        .keys()
        .copied()
        .chain(bundle.info.prerequisites.iter().copied())
        .collect();

    let mut dangling = BTreeSet::new();
    for (id, (kind, slice)) in &contents {
        let envelope = crate::envelope::ObjectEnvelope::decode(*kind, slice, limits.objects)
            .map_err(|e| ObjectError::Corrupt(format!("object {id} in bundle: {e}")))?;
        for dependency in envelope.dependencies {
            if !known.contains(&dependency) {
                dangling.insert(dependency);
            }
        }
    }

    let unsatisfied_roots = bundle
        .info
        .roots
        .iter()
        .filter(|(_, id)| !known.contains(id))
        .map(|(_, id)| *id)
        .collect();

    Ok(BundleVerification {
        info: bundle.info.clone(),
        objects_verified: contents.len() as u64,
        bytes_verified,
        unsatisfied_roots,
        dangling: dangling.into_iter().collect(),
    })
}

/// What an import did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub objects_added: u64,
    pub objects_already_present: u64,
    pub bytes_added: u64,
    /// Prerequisites the receiving store turned out not to have. A thin
    /// bundle imported against the wrong store leaves a gap, and this is it.
    pub missing_prerequisites: Vec<ObjectId>,
    pub refs: Vec<BundleRef>,
}

impl ImportReport {
    pub fn is_complete(&self) -> bool {
        self.missing_prerequisites.is_empty()
    }
}

/// Import a bundle into a store.
///
/// Verification happens first, over the whole file, before a single object is
/// written. A bundle that fails leaves the store untouched — which is why the
/// payload is checked in memory rather than streamed straight in.
pub fn import_bundle<S: ObjectStore + ?Sized>(
    path: &Path,
    store: &S,
    limits: &BundleLimits,
) -> Result<ImportReport> {
    let bundle = open(path, limits)?;
    let verification = verify_open(&bundle, limits)?;
    if !verification.dangling.is_empty() {
        return Err(ObjectError::Corrupt(format!(
            "bundle references {} objects it neither contains nor declares as prerequisites",
            verification.dangling.len()
        )));
    }

    let mut report = ImportReport {
        refs: bundle.info.refs.clone(),
        ..Default::default()
    };

    for id in &bundle.info.prerequisites {
        if !store.has_object(id)? {
            report.missing_prerequisites.push(*id);
        }
    }

    for (kind, id, offset, length) in &bundle.table {
        let slice = &bundle.payload[*offset as usize..(*offset + *length) as usize];
        if store.has_object(id)? {
            report.objects_already_present += 1;
            continue;
        }
        store.put_object(*kind, slice)?;
        report.objects_added += 1;
        report.bytes_added += *length;
    }

    Ok(report)
}

// ---------------------------------------------------------------------------

fn write_id(id: &ObjectId, out: &mut Vec<u8>) {
    out.push(id.algorithm.code());
    out.extend_from_slice(&id.digest);
}

fn read_id(input: &mut &[u8]) -> Result<ObjectId> {
    let code = take::<1>(input, "hash algorithm")?[0];
    let algorithm =
        HashAlgorithm::from_code(code).ok_or(ObjectError::UnknownHashAlgorithm(code))?;
    Ok(ObjectId::new(algorithm, take::<32>(input, "object id")?))
}

fn take<const N: usize>(input: &mut &[u8], what: &'static str) -> Result<[u8; N]> {
    if input.len() < N {
        return Err(ObjectError::Truncated(what));
    }
    let (head, rest) = input.split_at(N);
    *input = rest;
    let mut out = [0u8; N];
    out.copy_from_slice(head);
    Ok(out)
}

fn bounded(value: u64, max: usize, what: &'static str) -> Result<usize> {
    if value > max as u64 {
        return Err(ObjectError::TooLarge {
            what,
            len: value as usize,
            max,
        });
    }
    Ok(value as usize)
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension("cavsbundle.tmp");
    {
        let mut file = fs::File::create(&temporary).map_err(io_err("create bundle"))?;
        file.write_all(bytes).map_err(io_err("write bundle"))?;
        file.sync_all().map_err(io_err("sync bundle"))?;
    }
    fs::rename(&temporary, path).map_err(io_err("publish bundle"))?;
    Ok(())
}

fn io_err(what: &'static str) -> impl Fn(std::io::Error) -> ObjectError {
    move |source| ObjectError::Io { what, source }
}

/// Payload objects are read under the payload ceiling, structural ones under
/// the structural one.
fn limits_for(object: &StoredObject) -> DecodeLimits {
    DecodeLimits::DEFAULT.with_max_payload_len(object.bytes.len().max(1))
}

fn zstd_compress(raw: &[u8]) -> Result<Vec<u8>> {
    zstd::bulk::compress(raw, 3).map_err(io_err("compress bundle payload"))
}

fn zstd_decompress(stored: &[u8], expected: usize) -> Result<Vec<u8>> {
    // The capacity is the declared size, already checked against the reader's
    // budget, so a lying header cannot make this allocate without bound.
    let out = zstd::bulk::decompress(stored, expected)
        .map_err(|e| ObjectError::Corrupt(format!("bundle payload will not decompress: {e}")))?;
    if out.len() != expected {
        return Err(ObjectError::Corrupt(format!(
            "bundle payload decompressed to {} bytes, not the {expected} it declared",
            out.len()
        )));
    }
    Ok(out)
}

/// Objects a store holds, as a have-set for building a thin bundle.
pub fn have_set_of(objects: impl IntoIterator<Item = ObjectId>) -> BTreeSet<ObjectId> {
    objects.into_iter().collect()
}

/// Convenience: read one object out of a bundle without importing it.
pub fn read_from_bundle(
    path: &Path,
    id: &ObjectId,
    limits: &BundleLimits,
) -> Result<Option<StoredObject>> {
    let bundle = open(path, limits)?;
    for (kind, entry_id, offset, length) in &bundle.table {
        if entry_id != id {
            continue;
        }
        let bytes = bundle.payload[*offset as usize..(*offset + *length) as usize].to_vec();
        let actual = ObjectId::compute(*kind, &bytes);
        if actual != *id {
            return Err(ObjectError::IdMismatch {
                expected: id.to_hex(),
                actual: actual.to_hex(),
            });
        }
        return Ok(Some(StoredObject {
            id: *id,
            kind: *kind,
            bytes,
        }));
    }
    Ok(None)
}
