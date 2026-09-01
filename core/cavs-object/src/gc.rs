//! Deleting what nothing can reach.
//!
//! Collection is the one operation that can lose data, so the whole design is
//! about the cases where the obvious answer is wrong.
//!
//! An object is not garbage merely because nothing points at it *now*. A
//! writer part-way through a transaction has written its tree pages and not
//! yet moved the branch; a bundle import has staged its objects and not yet
//! offered its refs; a reader is holding ids it is about to fetch. Every one
//! of those looks exactly like garbage to a mark-and-sweep that only reads the
//! references.
//!
//! So three things protect an object beyond reachability: a grace period,
//! which keeps anything written recently; leases, which a caller takes over
//! ids it is working with; and a revalidation pass, which re-reads the roots
//! after the candidate list is built and refuses to delete if they moved. The
//! order matters — candidates are computed against a frozen view and confirmed
//! against a fresh one, so an object that became reachable in between is kept.
//!
//! Everything it does is reported: what was kept, why, and what was removed.

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime};

use crate::error::Result;
use crate::id::ObjectId;
use crate::store::{FsObjectStore, ObjectStore};
use crate::walk::{walk_reachable, GraphSource, WalkOptions};

/// How to run a collection.
#[derive(Debug, Clone)]
pub struct GcOptions {
    /// Objects younger than this are kept whatever the graph says. This is
    /// what stops a collection from deleting a half-finished transaction's
    /// work out from under it.
    pub grace: Duration,
    /// Ids a caller has claimed. A lease says "I am about to reference this",
    /// which the reference store cannot yet know.
    pub leases: BTreeSet<ObjectId>,
    /// Report what would happen and change nothing.
    pub dry_run: bool,
    pub walk: WalkOptions,
}

impl Default for GcOptions {
    fn default() -> Self {
        GcOptions {
            grace: Duration::from_secs(2 * 60 * 60),
            leases: BTreeSet::new(),
            dry_run: false,
            walk: WalkOptions::default(),
        }
    }
}

impl GcOptions {
    pub fn dry_run() -> Self {
        GcOptions {
            dry_run: true,
            ..Default::default()
        }
    }

    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    pub fn with_leases(mut self, leases: impl IntoIterator<Item = ObjectId>) -> Self {
        self.leases = leases.into_iter().collect();
        self
    }
}

/// What a collection did, in enough detail to explain any one decision.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    pub dry_run: bool,
    pub roots: u64,
    /// Objects a root can reach.
    pub reachable: u64,
    /// Unreachable, but kept because they were written inside the grace
    /// period.
    pub kept_by_grace: u64,
    /// Unreachable, but kept because a caller holds a lease on them.
    pub kept_by_lease: u64,
    pub removed: Vec<ObjectId>,
    pub bytes_removed: u64,
    /// Roots that are not in the store. A reference pointing at nothing is a
    /// problem to report, not a reason to collect everything else.
    pub missing_roots: Vec<ObjectId>,
    /// Set when the roots moved while the candidate list was being built, in
    /// which case nothing was deleted.
    pub aborted_because_roots_moved: bool,
}

impl GcReport {
    pub fn removed_count(&self) -> u64 {
        self.removed.len() as u64
    }
}

/// Collect a store down to what `roots` can reach.
///
/// `roots` is read twice: once to mark, and once more before anything is
/// deleted. If it changed in between, nothing is deleted and the report says
/// why — a collection that races a commit should lose the race, not the data.
pub fn collect(
    store: &FsObjectStore,
    read_roots: &dyn Fn() -> Result<Vec<ObjectId>>,
    options: &GcOptions,
) -> Result<GcReport> {
    let frozen = read_roots()?;
    let mut report = GcReport {
        dry_run: options.dry_run,
        roots: frozen.len() as u64,
        ..Default::default()
    };

    let mut reachable: BTreeSet<ObjectId> = BTreeSet::new();
    for visit in walk_reachable(store, &frozen, options.walk.clone())? {
        if visit.is_present() {
            reachable.insert(visit.id);
        } else {
            report.missing_roots.push(visit.id);
        }
    }
    report.reachable = reachable.len() as u64;

    let cutoff = SystemTime::now()
        .checked_sub(options.grace)
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let mut candidates: Vec<ObjectId> = Vec::new();
    for id in store.list_objects()? {
        if reachable.contains(&id) {
            continue;
        }
        if options.leases.contains(&id) {
            report.kept_by_lease += 1;
            continue;
        }
        if written_since(store, &id, cutoff) {
            report.kept_by_grace += 1;
            continue;
        }
        candidates.push(id);
    }

    if candidates.is_empty() {
        return Ok(report);
    }

    // Second look. Between the mark and here, a writer may have published a
    // reference to something on the candidate list; if the roots are not what
    // they were, this collection has no business deleting anything.
    let now = read_roots()?;
    if now != frozen {
        report.aborted_because_roots_moved = true;
        return Ok(report);
    }

    for id in candidates {
        // And one last check per object: a writer that raced the sweep leaves
        // a fresh mtime, and a fresh object is not garbage.
        if written_since(store, &id, cutoff) {
            report.kept_by_grace += 1;
            continue;
        }
        let size = store
            .get_object(&id)
            .map(|object| object.bytes.len() as u64)
            .unwrap_or(0);
        if !options.dry_run && !store.remove_object(&id)? {
            // Someone else got there first, which is fine.
            continue;
        }
        report.removed.push(id);
        report.bytes_removed += size;
    }

    Ok(report)
}

fn written_since(store: &FsObjectStore, id: &ObjectId, cutoff: SystemTime) -> bool {
    match store.object_written_at(id) {
        Ok(Some(written)) => written > cutoff,
        // No timestamp available: keeping it is the answer that cannot lose
        // anything.
        Ok(None) => true,
        Err(_) => true,
    }
}

/// Objects nothing can reach, without deleting any of them.
pub fn unreachable_objects<S: ObjectStore + GraphSource + ?Sized>(
    store: &S,
    roots: &[ObjectId],
    all: &[ObjectId],
    options: WalkOptions,
) -> Result<Vec<ObjectId>> {
    let mut reachable: BTreeSet<ObjectId> = BTreeSet::new();
    for visit in walk_reachable(store, roots, options)? {
        reachable.insert(visit.id);
    }
    Ok(all
        .iter()
        .copied()
        .filter(|id| !reachable.contains(id))
        .collect())
}
