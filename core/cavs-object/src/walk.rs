//! Walking the object graph.
//!
//! Everything CAVS does with a repository graph — deciding what to keep, what
//! to verify, what to pack, what to send — is the same question asked from
//! different roots: *which objects are reachable from here?* This module is
//! that question, and nothing more. It never looks inside a body.
//!
//! The traversal is depth-first over an explicit stack, so a deep graph costs
//! heap rather than call frames, and it is budgeted before it is started:
//! depth, fan-out, object count and byte count all have ceilings, and a
//! deadline or a cancel flag can stop it between any two objects. A graph
//! handed over by a stranger is exactly the input this has to survive.
//!
//! Content addressing makes a cycle infeasible — an object would have to name
//! a descendant that hashes back to it — but a source that does not verify
//! ids, such as a bundle being inspected before import, can present one. The
//! walk tracks its own path and reports a cycle as an error rather than
//! quietly terminating on it.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::error::{ObjectError, Result};
use crate::id::{ObjectId, ObjectKind};

/// One object, as the graph sees it: identity, class, size and edges. No body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectNode {
    pub id: ObjectId,
    pub kind: ObjectKind,
    /// Size of the object as stored, used to budget a walk and to size a
    /// transfer before it starts.
    pub stored_len: u64,
    pub dependencies: Vec<ObjectId>,
}

/// Somewhere object metadata can be looked up.
///
/// Deliberately narrower than a store: a walk needs edges and sizes, not
/// bytes, and a source that can answer without reading payload should.
pub trait GraphSource {
    /// Look up one object, or `Ok(None)` if the source does not have it.
    fn lookup(&self, id: &ObjectId) -> Result<Option<ObjectNode>>;
}

/// Which classes of object a walk cares about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KindFilter {
    /// Only these classes are yielded. Empty means every class.
    pub include: BTreeSet<ObjectKind>,
    /// These classes are never yielded, and never descended into.
    pub exclude: BTreeSet<ObjectKind>,
}

impl KindFilter {
    pub fn only(kinds: impl IntoIterator<Item = ObjectKind>) -> Self {
        KindFilter {
            include: kinds.into_iter().collect(),
            exclude: BTreeSet::new(),
        }
    }

    pub fn without(kinds: impl IntoIterator<Item = ObjectKind>) -> Self {
        KindFilter {
            include: BTreeSet::new(),
            exclude: kinds.into_iter().collect(),
        }
    }

    /// Metadata only: the structural graph, with payload left behind. This is
    /// what a metadata-only clone or a thin bundle asks for.
    pub fn metadata_only() -> Self {
        KindFilter::without([ObjectKind::Chunk])
    }

    pub fn admits(&self, kind: ObjectKind) -> bool {
        if self.exclude.contains(&kind) {
            return false;
        }
        self.include.is_empty() || self.include.contains(&kind)
    }

    pub fn is_unrestricted(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }
}

/// Ceilings and stop conditions for one walk.
#[derive(Debug, Clone)]
pub struct WalkOptions {
    pub max_depth: u32,
    pub max_dependencies_per_object: usize,
    pub max_objects: u64,
    pub max_bytes: u64,
    pub kinds: KindFilter,
    /// Treat an object the source does not have as an error. A local
    /// integrity walk wants this; a walk whose whole purpose is to find gaps
    /// does not.
    pub missing_is_error: bool,
    pub deadline: Option<Instant>,
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Default for WalkOptions {
    /// Defaults generous enough for a real repository and still finite for a
    /// hostile one.
    fn default() -> Self {
        WalkOptions {
            max_depth: 4096,
            max_dependencies_per_object: 1 << 20,
            max_objects: 100_000_000,
            max_bytes: 1 << 44,
            kinds: KindFilter::default(),
            missing_is_error: false,
            deadline: None,
            cancel: None,
        }
    }
}

impl WalkOptions {
    pub fn metadata_only() -> Self {
        WalkOptions {
            kinds: KindFilter::metadata_only(),
            ..Default::default()
        }
    }

    pub fn with_kinds(mut self, kinds: KindFilter) -> Self {
        self.kinds = kinds;
        self
    }

    pub fn requiring_every_object(mut self) -> Self {
        self.missing_is_error = true;
        self
    }
}

/// One step of a walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Visit {
    pub id: ObjectId,
    /// Hops from the nearest root; roots are at zero.
    pub depth: u32,
    /// `None` when the source does not have this object. Reachable-but-absent
    /// is a real state — a promised object, or a gap to fetch — and is not
    /// the same as corruption.
    pub node: Option<ObjectNode>,
}

impl Visit {
    pub fn is_present(&self) -> bool {
        self.node.is_some()
    }

    pub fn stored_len(&self) -> u64 {
        self.node.as_ref().map(|n| n.stored_len).unwrap_or(0)
    }
}

/// A walk frozen between two objects, so it can be picked up later.
#[derive(Debug, Clone, Default)]
pub struct WalkState {
    pending: Vec<(ObjectId, u32)>,
    visited: BTreeSet<ObjectId>,
    path: Vec<ObjectId>,
    objects: u64,
    bytes: u64,
}

impl WalkState {
    pub fn is_finished(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn visited_count(&self) -> usize {
        self.visited.len()
    }

    /// What the walk had yet to reach when it stopped.
    pub fn frontier(&self) -> Vec<ObjectId> {
        self.pending.iter().map(|(id, _)| *id).collect()
    }
}

/// A depth-first traversal of the reachable graph.
///
/// Yields each reachable object exactly once. Iteration ends after the first
/// error; a caller that wants to keep going past a gap should set
/// `missing_is_error` to false, which reports the gap as a visit rather than
/// as a failure.
pub struct Walk<'a, S: GraphSource + ?Sized> {
    source: &'a S,
    options: WalkOptions,
    state: WalkState,
    failed: bool,
    /// How many entries the most recent visit pushed, so its subtree can be
    /// pruned exactly without disturbing what other parents queued.
    pushed_last: usize,
}

impl<'a, S: GraphSource + ?Sized> Walk<'a, S> {
    pub fn new(source: &'a S, roots: &[ObjectId], options: WalkOptions) -> Self {
        // Reversed, so a stack pops the roots in the order they were given.
        let mut seen = BTreeSet::new();
        let pending: Vec<(ObjectId, u32)> = roots
            .iter()
            .rev()
            .filter(|id| seen.insert(**id))
            .map(|id| (*id, 0))
            .collect();
        Walk {
            source,
            options,
            state: WalkState {
                pending,
                ..Default::default()
            },
            failed: false,
            pushed_last: 0,
        }
    }

    /// Continue a walk that was paused.
    pub fn resume(source: &'a S, state: WalkState, options: WalkOptions) -> Self {
        Walk {
            source,
            options,
            state,
            failed: false,
            pushed_last: 0,
        }
    }

    /// Drop the subtree under the object just yielded.
    ///
    /// Only the entries that visit queued are removed — they are the top of
    /// the stack — so a child another parent also names stays reachable
    /// through that parent.
    pub fn prune_last(&mut self) {
        let keep = self.state.pending.len() - self.pushed_last;
        self.state.pending.truncate(keep);
        self.pushed_last = 0;
    }

    /// Stop, handing back enough to continue from exactly here.
    pub fn pause(self) -> WalkState {
        self.state
    }

    pub fn objects_visited(&self) -> u64 {
        self.state.objects
    }

    pub fn bytes_visited(&self) -> u64 {
        self.state.bytes
    }

    fn budget_check(&self) -> Result<()> {
        if let Some(cancel) = &self.options.cancel {
            if cancel.load(Ordering::Relaxed) {
                return Err(ObjectError::WalkCancelled);
            }
        }
        if let Some(deadline) = self.options.deadline {
            if Instant::now() >= deadline {
                return Err(ObjectError::WalkDeadlineExceeded);
            }
        }
        if self.state.objects > self.options.max_objects {
            return Err(ObjectError::WalkBudget {
                what: "objects",
                limit: self.options.max_objects,
            });
        }
        if self.state.bytes > self.options.max_bytes {
            return Err(ObjectError::WalkBudget {
                what: "bytes",
                limit: self.options.max_bytes,
            });
        }
        Ok(())
    }

    fn next_visit(&mut self) -> Option<Result<Visit>> {
        loop {
            if let Err(e) = self.budget_check() {
                return Some(Err(e));
            }

            // Unwind the ancestor path back to this object's parent before
            // descending again; that is what keeps cycle detection honest
            // across siblings.
            let (id, depth) = self.state.pending.pop()?;
            self.pushed_last = 0;
            // The ancestors of this object are exactly the first `depth`
            // entries of the path; anything past that belonged to a branch
            // the walk has already finished with.
            self.state.path.truncate(depth as usize);

            if !self.state.visited.insert(id) {
                continue;
            }

            if depth > self.options.max_depth {
                return Some(Err(ObjectError::WalkBudget {
                    what: "depth",
                    limit: self.options.max_depth as u64,
                }));
            }

            let node = match self.source.lookup(&id) {
                Ok(node) => node,
                Err(e) => return Some(Err(e)),
            };

            let Some(node) = node else {
                if self.options.missing_is_error {
                    return Some(Err(ObjectError::NotFound(id.to_hex())));
                }
                self.state.objects += 1;
                return Some(Ok(Visit {
                    id,
                    depth,
                    node: None,
                }));
            };

            if !self.options.kinds.admits(node.kind) {
                // Excluded classes are not yielded and not descended into:
                // that is what makes a metadata-only walk cheap rather than
                // merely quiet.
                continue;
            }

            if node.dependencies.len() > self.options.max_dependencies_per_object {
                return Some(Err(ObjectError::TooManyDependencies {
                    count: node.dependencies.len(),
                    max: self.options.max_dependencies_per_object,
                }));
            }

            self.state.objects += 1;
            self.state.bytes = self.state.bytes.saturating_add(node.stored_len);
            self.state.path.push(id);
            self.pushed_last = 0;
            for dep in node.dependencies.iter().rev() {
                // Checked here rather than on pop: an edge back into the
                // current path is a cycle even when the target was already
                // emitted and would otherwise just be skipped.
                if self.state.path.contains(dep) {
                    return Some(Err(ObjectError::CycleDetected(dep.to_hex())));
                }
                if !self.state.visited.contains(dep) {
                    self.state.pending.push((*dep, depth + 1));
                    self.pushed_last += 1;
                }
            }

            return Some(Ok(Visit {
                id,
                depth,
                node: Some(node),
            }));
        }
    }
}

impl<S: GraphSource + ?Sized> Iterator for Walk<'_, S> {
    type Item = Result<Visit>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let item = self.next_visit();
        if matches!(item, Some(Err(_))) {
            self.failed = true;
        }
        item
    }
}

/// Everything reachable from `roots`, as a set.
pub fn walk_reachable<S: GraphSource + ?Sized>(
    source: &S,
    roots: &[ObjectId],
    options: WalkOptions,
) -> Result<Vec<Visit>> {
    Walk::new(source, roots, options).collect()
}

/// What a receiver already holds.
///
/// A "have" is a promise about the object *and its closure*: claiming an
/// object means claiming everything it depends on. A sender may therefore
/// stop descending at a definite hit. An indefinite set — a Bloom filter —
/// makes no such promise, because a false positive would silently truncate
/// the transfer.
pub trait HaveSet {
    fn may_have(&self, id: &ObjectId) -> bool;

    /// True when a hit is certain. A Bloom filter answers false.
    fn is_definite(&self) -> bool {
        true
    }
}

/// An explicit list of ids, the exact form of a have-set.
impl HaveSet for BTreeSet<ObjectId> {
    fn may_have(&self, id: &ObjectId) -> bool {
        self.contains(id)
    }
}

/// A receiver that has nothing: the have-set of a fresh clone.
pub struct HaveNothing;

impl HaveSet for HaveNothing {
    fn may_have(&self, _id: &ObjectId) -> bool {
        false
    }
}

/// What has to be sent for a receiver to reach `roots`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MissingPlan {
    /// Objects the receiver certainly needs, in traversal order so a sender
    /// can stream them parents-last.
    pub missing: Vec<ObjectId>,
    /// Total stored size of `missing`.
    pub bytes: u64,
    /// Objects an indefinite have-set claimed. Each is either a real hit or a
    /// false positive, and the receiver has to say which before the transfer
    /// can be called complete.
    pub uncertain: Vec<ObjectId>,
    /// Reachable objects the *sender* does not have either. A plan with these
    /// cannot be fulfilled from this source alone.
    pub unavailable: Vec<ObjectId>,
    /// Objects skipped because the receiver definitely had them.
    pub already_present: u64,
}

impl MissingPlan {
    pub fn is_empty(&self) -> bool {
        self.missing.is_empty() && self.uncertain.is_empty()
    }

    pub fn is_fulfillable(&self) -> bool {
        self.unavailable.is_empty()
    }
}

/// Compute what `have` is missing of the graph under `roots`.
///
/// A definite hit prunes the subtree below it. An indefinite hit does not:
/// the object is recorded as uncertain and the walk continues underneath it,
/// so a Bloom false positive costs a confirmation round rather than an
/// incomplete reconstruction.
pub fn compute_missing<S: GraphSource + ?Sized>(
    source: &S,
    roots: &[ObjectId],
    have: &dyn HaveSet,
    options: WalkOptions,
) -> Result<MissingPlan> {
    let definite = have.is_definite();
    let mut plan = MissingPlan::default();
    let mut walk = Walk::new(source, roots, options);

    while let Some(visit) = walk.next() {
        let visit = visit?;
        let held = have.may_have(&visit.id);

        if held && definite {
            plan.already_present += 1;
            // Skip the subtree: claiming an object claims its closure.
            walk.prune_last();
            continue;
        }

        match &visit.node {
            None => plan.unavailable.push(visit.id),
            Some(node) => {
                if held {
                    plan.uncertain.push(visit.id);
                } else {
                    plan.missing.push(visit.id);
                    plan.bytes = plan.bytes.saturating_add(node.stored_len);
                }
            }
        }
    }

    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A graph held in memory, so a test can build shapes — including
    /// impossible ones like a cycle — that a verified store never would.
    #[derive(Default)]
    struct MemGraph {
        nodes: BTreeMap<ObjectId, ObjectNode>,
    }

    impl MemGraph {
        fn add(&mut self, name: u8, kind: ObjectKind, deps: &[u8], len: u64) -> ObjectId {
            let id = ObjectId::from_blake3([name; 32]);
            self.nodes.insert(
                id,
                ObjectNode {
                    id,
                    kind,
                    stored_len: len,
                    dependencies: deps
                        .iter()
                        .map(|d| ObjectId::from_blake3([*d; 32]))
                        .collect(),
                },
            );
            id
        }
    }

    impl GraphSource for MemGraph {
        fn lookup(&self, id: &ObjectId) -> Result<Option<ObjectNode>> {
            Ok(self.nodes.get(id).cloned())
        }
    }

    fn id(name: u8) -> ObjectId {
        ObjectId::from_blake3([name; 32])
    }

    /// commit 1 -> tree 2 -> {chunk 3, chunk 4}, and chunk 4 is shared with
    /// tree 5, which the same commit also names.
    fn diamond() -> MemGraph {
        let mut g = MemGraph::default();
        g.add(1, ObjectKind::Commit, &[2, 5], 100);
        g.add(2, ObjectKind::Tree, &[3, 4], 200);
        g.add(5, ObjectKind::Tree, &[4], 200);
        g.add(3, ObjectKind::Chunk, &[], 1000);
        g.add(4, ObjectKind::Chunk, &[], 2000);
        g
    }

    #[test]
    fn every_object_is_visited_exactly_once() {
        let g = diamond();
        let visits = walk_reachable(&g, &[id(1)], WalkOptions::default()).unwrap();
        let ids: Vec<ObjectId> = visits.iter().map(|v| v.id).collect();
        let unique: BTreeSet<ObjectId> = ids.iter().copied().collect();
        assert_eq!(ids.len(), 5);
        assert_eq!(unique.len(), 5, "the shared chunk was visited twice");
    }

    #[test]
    fn depth_is_the_shortest_path_taken() {
        let g = diamond();
        let visits = walk_reachable(&g, &[id(1)], WalkOptions::default()).unwrap();
        let depth = |want: ObjectId| visits.iter().find(|v| v.id == want).unwrap().depth;
        assert_eq!(depth(id(1)), 0);
        assert_eq!(depth(id(2)), 1);
        assert_eq!(depth(id(3)), 2);
    }

    #[test]
    fn bytes_and_objects_are_totalled() {
        let g = diamond();
        let mut walk = Walk::new(&g, &[id(1)], WalkOptions::default());
        while walk.next().transpose().unwrap().is_some() {}
        assert_eq!(walk.objects_visited(), 5);
        assert_eq!(walk.bytes_visited(), 100 + 200 + 200 + 1000 + 2000);
    }

    #[test]
    fn a_metadata_walk_never_touches_payload() {
        let g = diamond();
        let visits = walk_reachable(&g, &[id(1)], WalkOptions::metadata_only()).unwrap();
        assert_eq!(visits.len(), 3);
        assert!(visits
            .iter()
            .all(|v| v.node.as_ref().unwrap().kind != ObjectKind::Chunk));
    }

    #[test]
    fn a_cycle_is_an_error_not_a_hang() {
        let mut g = MemGraph::default();
        g.add(1, ObjectKind::Tree, &[2], 10);
        g.add(2, ObjectKind::Tree, &[3], 10);
        g.add(3, ObjectKind::Tree, &[1], 10);
        let err = walk_reachable(&g, &[id(1)], WalkOptions::default()).unwrap_err();
        assert!(matches!(err, ObjectError::CycleDetected(_)), "{err}");
    }

    #[test]
    fn a_self_reference_is_a_cycle() {
        let mut g = MemGraph::default();
        g.add(1, ObjectKind::Tree, &[1], 10);
        assert!(matches!(
            walk_reachable(&g, &[id(1)], WalkOptions::default()),
            Err(ObjectError::CycleDetected(_))
        ));
    }

    /// A diamond is not a cycle: the shared child is reached twice by
    /// different paths, and neither path contains the other.
    #[test]
    fn a_diamond_is_not_mistaken_for_a_cycle() {
        assert!(walk_reachable(&diamond(), &[id(1)], WalkOptions::default()).is_ok());
    }

    #[test]
    fn a_missing_object_is_reported_not_invented() {
        let mut g = MemGraph::default();
        g.add(1, ObjectKind::Tree, &[9], 10);
        let visits = walk_reachable(&g, &[id(1)], WalkOptions::default()).unwrap();
        assert_eq!(visits.len(), 2);
        assert!(!visits[1].is_present());

        let strict = walk_reachable(
            &g,
            &[id(1)],
            WalkOptions::default().requiring_every_object(),
        );
        assert!(matches!(strict, Err(ObjectError::NotFound(_))));
    }

    #[test]
    fn budgets_stop_the_walk() {
        let g = diamond();
        let capped = WalkOptions {
            max_objects: 2,
            ..Default::default()
        };
        assert!(matches!(
            walk_reachable(&g, &[id(1)], capped),
            Err(ObjectError::WalkBudget {
                what: "objects",
                ..
            })
        ));

        let byte_capped = WalkOptions {
            max_bytes: 150,
            ..Default::default()
        };
        assert!(matches!(
            walk_reachable(&g, &[id(1)], byte_capped),
            Err(ObjectError::WalkBudget { what: "bytes", .. })
        ));
    }

    #[test]
    fn depth_is_capped() {
        let mut g = MemGraph::default();
        for i in 1..10u8 {
            g.add(i, ObjectKind::Tree, &[i + 1], 1);
        }
        g.add(10, ObjectKind::Tree, &[], 1);
        let shallow = WalkOptions {
            max_depth: 3,
            ..Default::default()
        };
        assert!(matches!(
            walk_reachable(&g, &[id(1)], shallow),
            Err(ObjectError::WalkBudget { what: "depth", .. })
        ));
    }

    #[test]
    fn fan_out_is_capped() {
        let mut g = MemGraph::default();
        let deps: Vec<u8> = (10..40).collect();
        g.add(1, ObjectKind::Tree, &deps, 1);
        let narrow = WalkOptions {
            max_dependencies_per_object: 8,
            ..Default::default()
        };
        assert!(matches!(
            walk_reachable(&g, &[id(1)], narrow),
            Err(ObjectError::TooManyDependencies { .. })
        ));
    }

    #[test]
    fn cancellation_is_observed_between_objects() {
        let g = diamond();
        let flag = Arc::new(AtomicBool::new(false));
        let options = WalkOptions {
            cancel: Some(flag.clone()),
            ..Default::default()
        };
        let mut walk = Walk::new(&g, &[id(1)], options);
        assert!(walk.next().unwrap().is_ok());
        flag.store(true, Ordering::Relaxed);
        assert!(matches!(
            walk.next().unwrap(),
            Err(ObjectError::WalkCancelled)
        ));
    }

    #[test]
    fn a_walk_can_be_paused_and_continued() {
        let g = diamond();
        let mut walk = Walk::new(&g, &[id(1)], WalkOptions::default());
        let mut seen = vec![walk.next().unwrap().unwrap().id];
        seen.push(walk.next().unwrap().unwrap().id);
        let state = walk.pause();
        assert!(!state.is_finished());

        let resumed = Walk::resume(&g, state, WalkOptions::default());
        for visit in resumed {
            seen.push(visit.unwrap().id);
        }
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 5, "resuming re-walked or dropped objects");
    }

    #[test]
    fn an_exact_have_set_prunes_the_subtree() {
        let g = diamond();
        let have: BTreeSet<ObjectId> = [id(2)].into_iter().collect();
        let plan = compute_missing(&g, &[id(1)], &have, WalkOptions::default()).unwrap();
        // 1 and 5 and the chunk 5 names; 2's own children come with 2.
        assert_eq!(plan.missing, vec![id(1), id(5), id(4)]);
        assert_eq!(plan.already_present, 1);
        assert!(plan.uncertain.is_empty());
    }

    #[test]
    fn an_empty_have_set_needs_everything() {
        let plan =
            compute_missing(&diamond(), &[id(1)], &HaveNothing, WalkOptions::default()).unwrap();
        assert_eq!(plan.missing.len(), 5);
        assert_eq!(plan.bytes, 3500);
    }

    #[test]
    fn nothing_is_missing_when_the_root_is_held() {
        let have: BTreeSet<ObjectId> = [id(1)].into_iter().collect();
        let plan = compute_missing(&diamond(), &[id(1)], &have, WalkOptions::default()).unwrap();
        assert!(plan.is_empty());
        assert_eq!(plan.bytes, 0);
    }

    /// A Bloom filter cannot prune, or a false positive would truncate the
    /// transfer. Its hits become a confirmation list and the walk continues
    /// underneath them.
    #[test]
    fn an_indefinite_have_set_does_not_prune() {
        struct Maybe(BTreeSet<ObjectId>);
        impl HaveSet for Maybe {
            fn may_have(&self, id: &ObjectId) -> bool {
                self.0.contains(id)
            }
            fn is_definite(&self) -> bool {
                false
            }
        }
        // Claims tree 2 — falsely, as far as the transfer is concerned.
        let maybe = Maybe([id(2)].into_iter().collect());
        let plan = compute_missing(&diamond(), &[id(1)], &maybe, WalkOptions::default()).unwrap();
        assert_eq!(plan.uncertain, vec![id(2)]);
        // Everything under the claimed object is still accounted for.
        let all: BTreeSet<ObjectId> = plan
            .missing
            .iter()
            .chain(&plan.uncertain)
            .copied()
            .collect();
        assert_eq!(all.len(), 5);
    }

    #[test]
    fn an_unfulfillable_plan_says_so() {
        let mut g = MemGraph::default();
        g.add(1, ObjectKind::Tree, &[9], 10);
        let plan = compute_missing(&g, &[id(1)], &HaveNothing, WalkOptions::default()).unwrap();
        assert!(!plan.is_fulfillable());
        assert_eq!(plan.unavailable, vec![id(9)]);
    }
}
