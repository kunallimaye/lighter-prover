// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Backend-agnostic **atomic CAS object store** + **readiness-gating engine**.
//!
//! This module factors the readiness-gating algorithm (advance a parent's
//! completion count when a child commits; publish the parent's fold descriptor
//! **exactly once**, when the last child commits) out of any single backend so
//! it can be:
//!
//! * implemented **once**, correctly, and shared by both the dev/test
//!   [`crate::transport::LocalTransport`] (filesystem CAS) and the production
//!   [`crate::transport::pubsub::PubSubGcsTransport`] (GCS native-API CAS); and
//! * **unit-tested without any cloud** against the [`InMemoryCasStore`] double,
//!   which models exactly-one-winner compare-and-swap create-if-absent in
//!   process — the same contract GCS `ifGenerationMatch=0` provides.
//!
//! # Why the gate must be atomic CAS markers (not a counter you read+increment)
//!
//! In a distributed run two pods may, by design (Spot preemption, Pub/Sub
//! redelivery), commit the *same* child concurrently, and the *last* two
//! children of a node may commit on different nodes at the same instant. A naive
//! "read count, +1, write back" gate races: both last-children could read
//! `needed-1`, both write `needed`, and both publish the parent fold (a
//! double-publish), or a lost update could leave the parent never published. The
//! gate is therefore built from **idempotent atomic markers**:
//!
//! * one marker object per committed child — `gate/L{L}/N{n}/child_{idx}` —
//!   created via CAS create-if-absent. A redelivered child re-creates the *same*
//!   marker name and harmlessly observes [`CommitOutcome::AlreadyExists`], so it
//!   cannot double-count. The count is the number of distinct child markers.
//! * one publish marker per parent — `published/L{L}/N{n}` — also CAS-created.
//!   Whichever last-child wins that CAS is the **single** publisher of the
//!   parent fold; every other observes `AlreadyExists` and publishes nothing.
//!
//! Both markers rely only on the CAS create-if-absent primitive, which the
//! pilot verified is exactly-one-winner for GCS native `ifGenerationMatch=0`
//! (and is atomic on a single local filesystem for `O_EXCL`). The marker scheme
//! does not require a list/scan when the per-node child *quota* is known up
//! front (it is — `real_children_for_node`), so the gate needs only CAS-create
//! + a count of created markers, both expressible on the [`CasStore`] trait.

use super::{
    real_children_for_node, tree_depth, CommitOutcome, Role, WorkDescriptor,
};

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

// ─────────────────────────────────────────────────────────────────────────
// CAS object-store abstraction
// ─────────────────────────────────────────────────────────────────────────

/// An atomic **create-if-absent** object store: the single primitive the gating
/// engine and idempotent-output commit are built on.
///
/// The contract is intentionally tiny so it maps directly onto GCS native
/// `ifGenerationMatch=0` (the verified exactly-one-winner CAS) and onto a
/// filesystem `O_EXCL` create (atomic on one local FS). Implementations MUST
/// guarantee that, for concurrent [`cas_create`](CasStore::cas_create) calls on
/// the *same* key, **exactly one** returns [`CommitOutcome::Committed`] and
/// every other returns [`CommitOutcome::AlreadyExists`], with the stored bytes
/// being exactly the winner's (never interleaved).
pub trait CasStore: Send + Sync {
    /// Atomically create `key` with `bytes` iff it does not already exist.
    /// Exactly-one-winner. Returns `Committed` for the winner, `AlreadyExists`
    /// otherwise. Returns `Err` only for genuine I/O / transport failures (NOT
    /// for the already-exists precondition, which is a normal `AlreadyExists`).
    fn cas_create(&self, key: &str, bytes: &[u8]) -> Result<CommitOutcome, CasError>;

    /// Whether `key` exists.
    fn exists(&self, key: &str) -> Result<bool, CasError>;

    /// Read `key`'s bytes, or `None` if absent.
    fn read(&self, key: &str) -> Result<Option<Vec<u8>>, CasError>;

    /// Count existing objects whose key begins with `prefix`. Used to count the
    /// distinct child markers under a parent's gate directory. Implementations
    /// over a real bucket use a `list` with a prefix; the in-memory double scans
    /// its map.
    fn count_prefix(&self, prefix: &str) -> Result<usize, CasError>;
}

/// An error from a [`CasStore`] operation. Backends map their transport errors
/// into this; the already-exists precondition is NOT an error (it is a normal
/// [`CommitOutcome::AlreadyExists`]).
#[derive(Debug)]
pub struct CasError(pub String);

impl std::fmt::Display for CasError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CAS store error: {}", self.0)
    }
}

impl std::error::Error for CasError {}

/// A sink for follow-on [`WorkDescriptor`]s the gate publishes (e.g. a parent
/// fold once its last child commits). The production backend implements this by
/// publishing to a Pub/Sub topic; the local backend pushes onto its in-process
/// queue; tests use a recording fake.
pub trait Publisher: Send + Sync {
    /// Enqueue `descriptor` for some worker to pull. Idempotency of *output* is
    /// guaranteed downstream by CAS commit, but the gate already guarantees each
    /// parent fold is published **exactly once** via its publish marker, so a
    /// correct `Publisher` need not de-dupe.
    fn publish(&self, descriptor: WorkDescriptor) -> Result<(), CasError>;
}

// ─────────────────────────────────────────────────────────────────────────
// Gating engine
// ─────────────────────────────────────────────────────────────────────────

/// What the gate did in response to a child commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GatingOutcome {
    /// The committing caller had not won the CAS for this child (a redelivery /
    /// duplicate), so the gate did nothing.
    NotWinner,
    /// This child was recorded but the parent is not yet complete; nothing
    /// published.
    Recorded { have: usize, needed: usize },
    /// This child completed the parent's quota and THIS caller won the
    /// publish-marker CAS, so it published the parent fold exactly once.
    PublishedParent(WorkDescriptor),
    /// This child completed the parent's quota but ANOTHER concurrent
    /// last-child already won the publish-marker CAS, so this caller published
    /// nothing (the exactly-once guarantee in action).
    ParentAlreadyPublished,
    /// The child is the tree root (or beyond the top level): it has no parent to
    /// publish.
    RootReached,
}

/// The backend-agnostic readiness-gating engine. Given a [`CasStore`] for the
/// atomic markers and a [`Publisher`] for the follow-on folds, it implements
/// the exactly-once parent-publish invariant. Both [`LocalTransport`] and the
/// production [`PubSubGcsTransport`] drive this same engine, differing only in
/// which `CasStore`/`Publisher` they pass.
///
/// [`LocalTransport`]: crate::transport::LocalTransport
/// [`PubSubGcsTransport`]: crate::transport::pubsub::PubSubGcsTransport
pub struct GatingEngine<'a, S: CasStore, P: Publisher> {
    store: &'a S,
    publisher: &'a P,
}

impl<'a, S: CasStore, P: Publisher> GatingEngine<'a, S, P> {
    /// Build an engine over `store` (for markers) and `publisher` (for folds).
    pub fn new(store: &'a S, publisher: &'a P) -> Self {
        Self { store, publisher }
    }

    /// Marker key recording that `child_idx` (in its level's numbering) has
    /// committed under the parent at `(parent_level, parent_idx)`.
    fn child_marker_key(parent_level: usize, parent_idx: usize, child_idx: usize) -> String {
        format!("gate/L{parent_level}/N{parent_idx}/child_{child_idx}")
    }

    /// Prefix matching all child markers under a parent's gate.
    fn child_marker_prefix(parent_level: usize, parent_idx: usize) -> String {
        format!("gate/L{parent_level}/N{parent_idx}/child_")
    }

    /// Publish-marker key guaranteeing the parent fold is published once.
    fn publish_marker_key(parent_level: usize, parent_idx: usize) -> String {
        format!("published/L{parent_level}/N{parent_idx}")
    }

    /// Advance readiness for the parent of a just-committed `child` and, if the
    /// parent is now complete, publish the parent fold **exactly once**.
    ///
    /// `committed` indicates whether the caller WON the CAS commit of the child's
    /// *output* (only the winner advances the gate, so redeliveries — which
    /// observe `AlreadyExists` on the output commit — never inflate the count).
    /// This mirrors [`LocalTransport`]'s `maybe_publish_parent`, but on atomic
    /// CAS markers usable across nodes.
    ///
    /// [`LocalTransport`]: crate::transport::LocalTransport
    pub fn on_child_committed(
        &self,
        child: &WorkDescriptor,
        committed: CommitOutcome,
    ) -> Result<GatingOutcome, CasError> {
        if committed != CommitOutcome::Committed {
            return Ok(GatingOutcome::NotWinner);
        }

        // The child's (level, idx). Leaves are level 0; a level-L node's
        // children are level L-1.
        let (child_level, child_idx) = match child.role {
            Role::Leaf => (0usize, child.chunk_idx),
            Role::TreeNode => (child.level, child.node_idx),
            Role::RootCoordinator => return Ok(GatingOutcome::RootReached),
            // (#321 Phase 3) ReductionFold is gated by the interval-addressed
            // opportunistic adjacent-pair engine added in Phase 4, not this
            // fixed-node hex gating. No reduction descriptor is dispatched until
            // Phase 4 wires `--fold-strategy=reduction` into dispatch, so this
            // arm is unreached at runtime; treat as a no-op (nothing to gate).
            Role::ReductionFold => return Ok(GatingOutcome::Recorded { have: 0, needed: 0 }),
        };

        let radix = child.radix;
        let leaf_count = child.leaf_count;
        let depth = tree_depth(leaf_count, radix);
        let parent_level = child_level + 1;
        if parent_level > depth {
            // The child is the root; nothing above it to publish.
            return Ok(GatingOutcome::RootReached);
        }
        let parent_idx = child_idx / radix;
        let needed = real_children_for_node(leaf_count, radix, parent_level, parent_idx);

        // Record this child's completion (idempotent via the CAS marker name).
        let marker = Self::child_marker_key(parent_level, parent_idx, child_idx);
        // A re-commit that somehow reaches here (shouldn't, since only the output
        // CAS winner calls in) is still safe: same marker name => AlreadyExists.
        let _ = self.store.cas_create(&marker, b"1")?;

        // Count distinct committed children for the parent.
        let have = self
            .store
            .count_prefix(&Self::child_marker_prefix(parent_level, parent_idx))?;

        if have < needed {
            return Ok(GatingOutcome::Recorded { have, needed });
        }

        // Parent quota met: publish the parent fold EXACTLY once, guarded by the
        // publish marker. Whichever last-child wins this CAS is the sole
        // publisher.
        let pub_marker = Self::publish_marker_key(parent_level, parent_idx);
        match self.store.cas_create(&pub_marker, b"1")? {
            CommitOutcome::Committed => {
                let fold = WorkDescriptor::tree_node(
                    parent_level,
                    parent_idx,
                    radix,
                    leaf_count,
                    child.tx_per_proof,
                );
                self.publisher.publish(fold.clone())?;
                Ok(GatingOutcome::PublishedParent(fold))
            }
            CommitOutcome::AlreadyExists => Ok(GatingOutcome::ParentAlreadyPublished),
        }
    }

    // ─────────────────────────────────────────────────────────────────────
    // Order-free adjacent-pair (interval) gating — issue #321 Phase 4
    //
    // The hex `on_child_committed` above waits for ALL `radix` children of a
    // fixed `(level, node_idx)` node before publishing that node. The reduction
    // path instead merges ADJACENT same-height intervals opportunistically, the
    // moment BOTH members of a pair are committed — so folding overlaps leaf
    // proving and a straggler only delays its own pair, not the whole level.
    //
    // TWO correctness hazards and how this design defeats them:
    //
    // 1. DOUBLE-CONSUMPTION RACE (the silent-deadlock hazard). If an interval
    //    could be merged by EITHER neighbour, two concurrent commits could each
    //    claim it into a DIFFERENT merged interval (different CAS keys, both
    //    succeed) — consuming it twice and producing overlapping intervals that
    //    can never span [0, N-1]: a silent deadlock, no root.
    //    DEFEAT: a DETERMINISTIC PAIRING RULE. In a same-height binary tree,
    //    every interval is either the LEFT or the RIGHT child of its unique
    //    parent pair. Its partner is FIXED by position (not "whichever neighbour
    //    commits first"), so an interval belongs to EXACTLY ONE pair and can be
    //    consumed by EXACTLY ONE merge. Whichever member commits SECOND finds the
    //    partner present and drives the (single, deterministic) merged interval
    //    [left.lo, right.hi]; the merge identity is independent of arrival order.
    //
    // 2. EXACTLY-ONCE + RECOVERY. The merged parent is published under a CAS
    //    publish-marker keyed by the MERGED interval, so even if both members
    //    race to drive the same pair only one publishes. The marker VALUE carries
    //    a lease stamp so a lost/timed-out merge task can be re-driven (Phase 5
    //    coordinator), rather than the seam being stuck forever (the
    //    TTL-is-GC-not-lease hazard).
    // ─────────────────────────────────────────────────────────────────────

    /// Marker recording that the interval `[lo, hi]` (span `2^level`) has
    /// committed. Interval-addressed, independent of any radix-node slot.
    fn interval_marker_key(lo: usize, hi: usize) -> String {
        format!("rgate/committed/{lo}_{hi}")
    }

    /// Publish-marker guaranteeing the merged parent `[lo, hi]` is published
    /// once. Its value is a lease stamp (millis) for crash-recovery re-drive.
    fn merge_publish_marker_key(lo: usize, hi: usize) -> String {
        format!("rgate/published/{lo}_{hi}")
    }

    /// Given a committed same-height interval `[lo, hi]` (span `s = hi-lo+1 =
    /// 2^level`), return `(is_right_child, partner)` where `partner` is the
    /// adjacent SAME-HEIGHT sibling it pairs with under its parent:
    ///   * right child (odd block index `lo/s`): partner = the LEFT sibling
    ///     `[lo - s, lo - 1]`.
    ///   * left child (even block index): partner = the RIGHT sibling
    ///     `[hi + 1, hi + s]`.
    /// The merged parent is always `[left.lo, right.hi]` regardless of which
    /// member calls, giving every pair a single deterministic merge identity.
    fn sibling_of(lo: usize, hi: usize) -> (bool, (usize, usize)) {
        let span = hi - lo + 1;
        let block = lo / span; // index of this interval among same-height blocks
        if block % 2 == 1 {
            (true, (lo - span, lo - 1)) // right child → left sibling
        } else {
            (false, (hi + 1, hi + span)) // left child → right sibling
        }
    }

    /// Advance order-free reduction gating for a just-committed reduction/leaf
    /// interval and, when its adjacent same-height partner is also present,
    /// publish the merged parent fold EXACTLY once.
    ///
    /// `committed` must be the output-commit CAS outcome (only the output winner
    /// drives the gate, so redeliveries never double-drive). A leaf `i` is
    /// `[i, i]` at level 0; a reduction fold output is `[lo, hi]` at its level.
    ///
    /// Deterministic pairing (see the module note) guarantees no interval is
    /// consumed by two merges, so overlapping-interval deadlock cannot occur.
    #[allow(clippy::too_many_arguments)]
    pub fn on_interval_committed(
        &self,
        lo: usize,
        hi: usize,
        level: usize,
        radix: usize,
        leaf_count: usize,
        tx_per_proof: usize,
        committed: CommitOutcome,
    ) -> Result<GatingOutcome, CasError> {
        if committed != CommitOutcome::Committed {
            return Ok(GatingOutcome::NotWinner);
        }

        // Record THIS interval as committed (idempotent via the marker name).
        let _ = self
            .store
            .cas_create(&Self::interval_marker_key(lo, hi), b"1")?;

        // The whole tree spans [0, leaf_count-1]. If this interval already spans
        // it, it is the root — nothing above to merge.
        if lo == 0 && hi == leaf_count - 1 {
            return Ok(GatingOutcome::RootReached);
        }

        let (is_right, (plo, phi)) = Self::sibling_of(lo, hi);
        let (mlo, mhi) = if is_right { (plo, hi) } else { (lo, phi) };
        let merged_level = level + 1;

        // Is the partner committed yet? If not, record + wait; the partner's own
        // commit will re-evaluate this same pair. This symmetric check fires the
        // merge the moment the SECOND of the pair lands, any arrival order — the
        // deterministic [left,right] merged key gives it a single identity.
        let partner_present = self
            .store
            .exists(&Self::interval_marker_key(plo, phi))?;
        if !partner_present {
            return Ok(GatingOutcome::Recorded { have: 1, needed: 2 });
        }

        // Both present → publish the merged parent EXACTLY once, guarded by the
        // merged-interval publish marker (value = lease stamp for Phase-5
        // crash-recovery re-drive). Whichever of the pair wins the CAS is the
        // sole publisher; the other observes AlreadyExists.
        let pub_marker = Self::merge_publish_marker_key(mlo, mhi);
        let lease_stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
            .to_string();
        match self.store.cas_create(&pub_marker, lease_stamp.as_bytes())? {
            CommitOutcome::Committed => {
                let fold = WorkDescriptor::reduction_fold(
                    mlo,
                    mhi,
                    merged_level,
                    radix,
                    leaf_count,
                    tx_per_proof,
                );
                self.publisher.publish(fold.clone())?;
                Ok(GatingOutcome::PublishedParent(fold))
            }
            CommitOutcome::AlreadyExists => Ok(GatingOutcome::ParentAlreadyPublished),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// InMemoryCasStore — exactly-one-winner CAS double (test / no-cloud)
// ─────────────────────────────────────────────────────────────────────────

/// An in-process [`CasStore`] modelling GCS `ifGenerationMatch=0`
/// exactly-one-winner semantics. Used to unit-test the gating engine and the
/// idempotent-commit logic with **no network**. Cloneable; clones share state
/// (an `Arc<Mutex<..>>`), so it can be used across threads to exercise races.
#[derive(Clone, Default)]
pub struct InMemoryCasStore {
    inner: Arc<Mutex<std::collections::HashMap<String, Vec<u8>>>>,
}

impl InMemoryCasStore {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of objects currently stored (test convenience).
    pub fn len(&self) -> usize {
        self.inner.lock().expect("cas mutex poisoned").len()
    }

    /// Whether the store is empty (test convenience).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl CasStore for InMemoryCasStore {
    fn cas_create(&self, key: &str, bytes: &[u8]) -> Result<CommitOutcome, CasError> {
        let mut map = self.inner.lock().expect("cas mutex poisoned");
        // The whole check-and-insert is under one lock => atomic, exactly-one
        // winner, just like the GCS `ifGenerationMatch=0` precondition.
        if map.contains_key(key) {
            Ok(CommitOutcome::AlreadyExists)
        } else {
            map.insert(key.to_string(), bytes.to_vec());
            Ok(CommitOutcome::Committed)
        }
    }

    fn exists(&self, key: &str) -> Result<bool, CasError> {
        Ok(self
            .inner
            .lock()
            .expect("cas mutex poisoned")
            .contains_key(key))
    }

    fn read(&self, key: &str) -> Result<Option<Vec<u8>>, CasError> {
        Ok(self
            .inner
            .lock()
            .expect("cas mutex poisoned")
            .get(key)
            .cloned())
    }

    fn count_prefix(&self, prefix: &str) -> Result<usize, CasError> {
        Ok(self
            .inner
            .lock()
            .expect("cas mutex poisoned")
            .keys()
            .filter(|k| k.starts_with(prefix))
            .count())
    }
}

/// A recording [`Publisher`] for tests: collects every published descriptor so a
/// test can assert exactly-once publication. Cloneable; clones share state.
#[derive(Clone, Default)]
pub struct RecordingPublisher {
    published: Arc<Mutex<Vec<WorkDescriptor>>>,
}

impl RecordingPublisher {
    /// Empty recorder.
    pub fn new() -> Self {
        Self::default()
    }

    /// A snapshot of all descriptors published so far, in order.
    pub fn published(&self) -> Vec<WorkDescriptor> {
        self.published
            .lock()
            .expect("publisher mutex poisoned")
            .clone()
    }

    /// The set of distinct fold output-keys published (so a test can assert no
    /// key was published twice even under concurrency).
    pub fn distinct_keys(&self) -> HashSet<String> {
        self.published()
            .iter()
            .map(|d| d.output_key())
            .collect()
    }
}

impl Publisher for RecordingPublisher {
    fn publish(&self, descriptor: WorkDescriptor) -> Result<(), CasError> {
        self.published
            .lock()
            .expect("publisher mutex poisoned")
            .push(descriptor);
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Tests — gating + CAS, all cloud-free against the in-memory double
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    #[test]
    fn in_memory_cas_is_exactly_one_winner_single_thread() {
        let s = InMemoryCasStore::new();
        assert_eq!(s.cas_create("k", b"a").unwrap(), CommitOutcome::Committed);
        assert_eq!(
            s.cas_create("k", b"b").unwrap(),
            CommitOutcome::AlreadyExists
        );
        // Winner's bytes survive.
        assert_eq!(s.read("k").unwrap().unwrap(), b"a");
        assert!(s.exists("k").unwrap());
        assert!(!s.exists("missing").unwrap());
    }

    #[test]
    fn in_memory_cas_exactly_one_winner_under_concurrency() {
        let s = InMemoryCasStore::new();
        const N: usize = 32;
        let committed = Arc::new(AtomicUsize::new(0));
        let already = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for i in 0..N {
            let s = s.clone();
            let c = committed.clone();
            let a = already.clone();
            handles.push(thread::spawn(move || {
                let payload = format!("winner-{i}");
                match s.cas_create("shared", payload.as_bytes()).unwrap() {
                    CommitOutcome::Committed => {
                        c.fetch_add(1, Ordering::SeqCst);
                    }
                    CommitOutcome::AlreadyExists => {
                        a.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(committed.load(Ordering::SeqCst), 1, "exactly one CAS winner");
        assert_eq!(already.load(Ordering::SeqCst), N - 1);
        let stored = String::from_utf8(s.read("shared").unwrap().unwrap()).unwrap();
        assert!(stored.starts_with("winner-"));
    }

    #[test]
    fn count_prefix_counts_only_matching_keys() {
        let s = InMemoryCasStore::new();
        s.cas_create("gate/L1/N0/child_0", b"1").unwrap();
        s.cas_create("gate/L1/N0/child_1", b"1").unwrap();
        s.cas_create("gate/L1/N1/child_0", b"1").unwrap();
        s.cas_create("published/L1/N0", b"1").unwrap();
        assert_eq!(s.count_prefix("gate/L1/N0/child_").unwrap(), 2);
        assert_eq!(s.count_prefix("gate/L1/N1/child_").unwrap(), 1);
        assert_eq!(s.count_prefix("gate/L9/N9/child_").unwrap(), 0);
    }

    #[test]
    fn gating_publishes_parent_when_children_complete() {
        // radix=2, N=4 => level-1 node 0 folds leaves {0,1}.
        let store = InMemoryCasStore::new();
        let pubr = RecordingPublisher::new();
        let engine = GatingEngine::new(&store, &pubr);

        let l0 = WorkDescriptor::leaf(0, 2, 4, 1);
        let l1 = WorkDescriptor::leaf(1, 2, 4, 1);

        // First child: recorded, nothing published.
        let o0 = engine
            .on_child_committed(&l0, CommitOutcome::Committed)
            .unwrap();
        assert_eq!(o0, GatingOutcome::Recorded { have: 1, needed: 2 });
        assert!(pubr.published().is_empty());

        // Second child: parent complete, fold published exactly once.
        let o1 = engine
            .on_child_committed(&l1, CommitOutcome::Committed)
            .unwrap();
        match o1 {
            GatingOutcome::PublishedParent(d) => {
                assert_eq!(d.role, Role::TreeNode);
                assert_eq!(d.level, 1);
                assert_eq!(d.node_idx, 0);
            }
            other => panic!("expected PublishedParent, got {other:?}"),
        }
        assert_eq!(pubr.published().len(), 1);
    }

    #[test]
    fn gating_redelivered_child_does_not_double_count() {
        // A child whose output commit was AlreadyExists (redelivery) must NOT
        // advance the gate.
        let store = InMemoryCasStore::new();
        let pubr = RecordingPublisher::new();
        let engine = GatingEngine::new(&store, &pubr);

        let l0 = WorkDescriptor::leaf(0, 2, 4, 1);
        assert_eq!(
            engine
                .on_child_committed(&l0, CommitOutcome::Committed)
                .unwrap(),
            GatingOutcome::Recorded { have: 1, needed: 2 }
        );
        // Redelivery: output commit said AlreadyExists => NotWinner, no advance.
        assert_eq!(
            engine
                .on_child_committed(&l0, CommitOutcome::AlreadyExists)
                .unwrap(),
            GatingOutcome::NotWinner
        );
        // Parent still needs leaf 1 => nothing published.
        assert!(pubr.published().is_empty());
        assert_eq!(store.count_prefix("gate/L1/N0/child_").unwrap(), 1);
    }

    #[test]
    fn gating_root_child_has_no_parent() {
        // radix=2, N=4 => depth 2; the level-2 node IS the root.
        let store = InMemoryCasStore::new();
        let pubr = RecordingPublisher::new();
        let engine = GatingEngine::new(&store, &pubr);
        let root_node = WorkDescriptor::tree_node(2, 0, 2, 4, 1);
        assert_eq!(
            engine
                .on_child_committed(&root_node, CommitOutcome::Committed)
                .unwrap(),
            GatingOutcome::RootReached
        );
        assert!(pubr.published().is_empty());
    }

    #[test]
    fn gating_level1_nodes_publish_root_fold() {
        // radix=2, N=4 => level-1 nodes {0,1} fold into the level-2 root.
        let store = InMemoryCasStore::new();
        let pubr = RecordingPublisher::new();
        let engine = GatingEngine::new(&store, &pubr);
        let n0 = WorkDescriptor::tree_node(1, 0, 2, 4, 1);
        let n1 = WorkDescriptor::tree_node(1, 1, 2, 4, 1);

        assert_eq!(
            engine
                .on_child_committed(&n0, CommitOutcome::Committed)
                .unwrap(),
            GatingOutcome::Recorded { have: 1, needed: 2 }
        );
        match engine
            .on_child_committed(&n1, CommitOutcome::Committed)
            .unwrap()
        {
            GatingOutcome::PublishedParent(d) => {
                assert_eq!(d.level, 2);
                assert_eq!(d.node_idx, 0);
            }
            other => panic!("expected root fold published, got {other:?}"),
        }
    }

    /// The acceptance-criteria concurrency test: many threads commit the SAME
    /// child and the two distinct last-children concurrently; the parent fold is
    /// published **exactly once** regardless of races.
    #[test]
    fn gating_exactly_once_under_concurrency() {
        // radix=2, N=2 => one level-1 root folding leaves {0,1}.
        let store = InMemoryCasStore::new();
        let pubr = RecordingPublisher::new();

        const THREADS_PER_CHILD: usize = 16;
        let mut handles = Vec::new();
        for child_idx in 0..2usize {
            for _ in 0..THREADS_PER_CHILD {
                let store = store.clone();
                let pubr = pubr.clone();
                handles.push(thread::spawn(move || {
                    let engine = GatingEngine::new(&store, &pubr);
                    let leaf = WorkDescriptor::leaf(child_idx, 2, 2, 1);
                    // Each thread races the OUTPUT-commit CAS for its child key,
                    // then drives gating with the real outcome — exactly mirroring
                    // how the transport calls it (only the output-CAS winner
                    // advances the gate).
                    let output_key = leaf.output_key();
                    let outcome = store.cas_create(&output_key, b"proof").unwrap();
                    engine.on_child_committed(&leaf, outcome).unwrap();
                }));
            }
        }
        for h in handles {
            h.join().unwrap();
        }

        // Exactly one parent fold published, despite 32 racing threads.
        let published = pubr.published();
        assert_eq!(
            published.len(),
            1,
            "parent fold must be published exactly once, got {published:?}"
        );
        assert_eq!(pubr.distinct_keys().len(), 1);
        let d = &published[0];
        assert_eq!(d.role, Role::TreeNode);
        assert_eq!(d.level, 1);
        assert_eq!(d.node_idx, 0);
    }

    /// Full radix-2 N=4 tree driven purely through the gating engine + CAS double:
    /// 4 leaves => 2 level-1 folds => 1 root fold, each published exactly once.
    #[test]
    fn gating_full_radix2_n4_tree_each_fold_once() {
        let store = InMemoryCasStore::new();
        let pubr = RecordingPublisher::new();
        let engine = GatingEngine::new(&store, &pubr);

        // Commit 4 leaves.
        for i in 0..4 {
            let leaf = WorkDescriptor::leaf(i, 2, 4, 1);
            let outcome = store.cas_create(&leaf.output_key(), b"leaf").unwrap();
            engine.on_child_committed(&leaf, outcome).unwrap();
        }
        // Two level-1 folds should now be published.
        let after_leaves = pubr.published();
        assert_eq!(after_leaves.len(), 2, "two level-1 folds, got {after_leaves:?}");

        // Commit the two level-1 node outputs (simulating those folds completing).
        for n in 0..2 {
            let node = WorkDescriptor::tree_node(1, n, 2, 4, 1);
            let outcome = store.cas_create(&node.output_key(), b"node").unwrap();
            engine.on_child_committed(&node, outcome).unwrap();
        }
        // Now the single root fold (level 2) is published.
        let all = pubr.published();
        assert_eq!(all.len(), 3, "2 level-1 + 1 root fold, got {all:?}");
        let root = all.last().unwrap();
        assert_eq!(root.level, 2);
        assert_eq!(root.node_idx, 0);

        // Every published key is distinct (exactly-once across the whole tree).
        assert_eq!(pubr.distinct_keys().len(), 3);
    }

    // ── Order-free adjacent-pair (interval) gating — #321 Phase 4 ────────────

    /// Helper: drive an interval commit through the engine (radix 2, tx 1).
    fn commit_interval(
        eng: &GatingEngine<InMemoryCasStore, RecordingPublisher>,
        lo: usize,
        hi: usize,
        level: usize,
        leaf_count: usize,
    ) -> GatingOutcome {
        eng.on_interval_committed(lo, hi, level, 2, leaf_count, 1, CommitOutcome::Committed)
            .unwrap()
    }

    /// The merge fires the moment the SECOND member of a pair lands, regardless
    /// of arrival order, and publishes the merged interval exactly once — for
    /// BOTH orders (left-then-right and right-then-left).
    #[test]
    fn adjacent_pair_merges_on_second_commit_either_order() {
        for right_first in [false, true] {
            let store = InMemoryCasStore::new();
            let pubr = RecordingPublisher::new();
            let eng = GatingEngine::new(&store, &pubr);
            let (first, second) = if right_first {
                ((1usize, 1usize), (0usize, 0usize))
            } else {
                ((0usize, 0usize), (1usize, 1usize))
            };
            let o1 = commit_interval(&eng, first.0, first.1, 0, 2);
            assert!(
                matches!(o1, GatingOutcome::Recorded { .. }),
                "first commit (right_first={right_first}) must only record: {o1:?}"
            );
            let o2 = commit_interval(&eng, second.0, second.1, 0, 2);
            match o2 {
                GatingOutcome::PublishedParent(d) => {
                    assert_eq!(d.output_key(), "reduction_0_1.proof");
                    assert_eq!(d.level, 1);
                }
                other => panic!("second commit must publish merged [0,1]: {other:?}"),
            }
            assert_eq!(pubr.published().len(), 1, "exactly one merge published");
        }
    }

    /// THE double-consumption race guard. Three+ adjacent committed intervals
    /// where a naive impl could merge the mis-aligned [1,2] pair. The
    /// deterministic pairing rule must yield only the valid [0,1] and [2,3]
    /// merges — never [1,2] — even under an adversarial commit order.
    #[test]
    fn no_double_consumption_three_adjacent_intervals() {
        let store = InMemoryCasStore::new();
        let pubr = RecordingPublisher::new();
        let eng = GatingEngine::new(&store, &pubr);
        // Adversarial: the two MIDDLE leaves first (adjacent; would tempt [1,2]).
        commit_interval(&eng, 1, 1, 0, 4);
        commit_interval(&eng, 2, 2, 0, 4);
        commit_interval(&eng, 0, 0, 0, 4);
        commit_interval(&eng, 3, 3, 0, 4);

        let keys = pubr.distinct_keys();
        assert!(keys.contains("reduction_0_1.proof"), "must merge [0,1]");
        assert!(keys.contains("reduction_2_3.proof"), "must merge [2,3]");
        assert!(
            !keys.contains("reduction_1_2.proof"),
            "MUST NOT merge mis-aligned [1,2] (double-consumption / overlap!)"
        );
        assert_eq!(pubr.published().len(), 2, "exactly two level-1 merges");
    }

    /// A full same-height reduction over N=4 reaches the single root [0,3] with
    /// no overlaps. Drives level-1 outputs back in as commits (as Phase 5 will).
    #[test]
    fn full_reduction_n4_reaches_root() {
        let store = InMemoryCasStore::new();
        let pubr = RecordingPublisher::new();
        let eng = GatingEngine::new(&store, &pubr);
        for i in 0..4 {
            commit_interval(&eng, i, i, 0, 4);
        }
        let a = commit_interval(&eng, 0, 1, 1, 4);
        assert!(matches!(a, GatingOutcome::Recorded { .. }), "[0,1] waits for [2,3]");
        let b = commit_interval(&eng, 2, 3, 1, 4);
        match b {
            GatingOutcome::PublishedParent(d) => {
                assert_eq!(d.output_key(), "reduction_0_3.proof");
                assert_eq!(d.level, 2);
            }
            other => panic!("[2,3] commit must publish root [0,3]: {other:?}"),
        }
        let r = commit_interval(&eng, 0, 3, 2, 4);
        assert_eq!(r, GatingOutcome::RootReached);
    }

    /// Exactly-once merge publication under concurrency: both members of a pair
    /// commit simultaneously; exactly one publishes the merged parent.
    #[test]
    fn merge_published_exactly_once_under_concurrency() {
        let store = InMemoryCasStore::new();
        let pubr = RecordingPublisher::new();
        type G<'a> = GatingEngine<'a, InMemoryCasStore, RecordingPublisher>;
        // Pre-record BOTH members so both drivers see the partner and race.
        store.cas_create(&G::interval_marker_key(0, 0), b"1").unwrap();
        store.cas_create(&G::interval_marker_key(1, 1), b"1").unwrap();

        let published = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for who in [(0usize, 0usize), (1usize, 1usize)] {
            let store = store.clone();
            let pubr = pubr.clone();
            let pc = published.clone();
            handles.push(thread::spawn(move || {
                let eng = GatingEngine::new(&store, &pubr);
                if let GatingOutcome::PublishedParent(_) = eng
                    .on_interval_committed(who.0, who.1, 0, 2, 2, 1, CommitOutcome::Committed)
                    .unwrap()
                {
                    pc.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }
        for h in handles { h.join().unwrap(); }
        assert_eq!(published.load(Ordering::SeqCst), 1, "exactly one merge publish");
        assert_eq!(pubr.published().len(), 1, "exactly one descriptor published");
    }

    /// `sibling_of` computes the correct same-height partner + initiator flag.
    #[test]
    fn sibling_of_is_correct() {
        type G<'a> = GatingEngine<'a, InMemoryCasStore, RecordingPublisher>;
        assert_eq!(G::sibling_of(0, 0), (false, (1, 1)));
        assert_eq!(G::sibling_of(1, 1), (true, (0, 0)));
        assert_eq!(G::sibling_of(2, 3), (true, (0, 1)));
        assert_eq!(G::sibling_of(0, 1), (false, (2, 3)));
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Redis CAS Store Implementation (GCP Memorystore)
// ─────────────────────────────────────────────────────────────────────────

#[cfg(feature = "pubsub")]
pub struct RedisCasStore {
    client: redis::Client,
    ttl_secs: usize,
}

#[cfg(feature = "pubsub")]
impl RedisCasStore {
    /// Connect to Redis using the provided connection string (e.g., "redis://127.0.0.1:6379").
    pub fn new(connection_info: &str, ttl_secs: usize) -> Result<Self, CasError> {
        let client = redis::Client::open(connection_info)
            .map_err(|e| CasError(format!("redis open: {e}")))?;
        Ok(Self { client, ttl_secs })
    }

    fn get_connection(&self) -> Result<redis::Connection, CasError> {
        self.client
            .get_connection()
            .map_err(|e| CasError(format!("redis connect: {e}")))
    }
}

#[cfg(feature = "pubsub")]
impl CasStore for RedisCasStore {
    fn cas_create(&self, key: &str, bytes: &[u8]) -> Result<CommitOutcome, CasError> {
        use redis::Commands as _;
        let mut conn = self.get_connection()?;

        if key.starts_with("gate/") {
            // key format: gate/L{level}/N{node_idx}/child_{child_idx}
            // We map to SADD gate:L{level}:N{node_idx} {child_idx}
            let parts: Vec<&str> = key.split('/').collect();
            if parts.len() == 4 && parts[3].starts_with("child_") {
                let level_str = parts[1];
                let node_str = parts[2];
                let child_idx_str = &parts[3]["child_".len()..];
                
                let redis_key = format!("gate:{}:{}", level_str, node_str);
                let added: i32 = conn.sadd(&redis_key, child_idx_str)
                    .map_err(|e| CasError(format!("redis sadd: {e}")))?;
                
                let _: () = conn.expire(&redis_key, self.ttl_secs as i64)
                    .map_err(|e| CasError(format!("redis expire: {e}")))?;

                if added == 1 {
                    Ok(CommitOutcome::Committed)
                } else {
                    Ok(CommitOutcome::AlreadyExists)
                }
            } else {
                Err(CasError(format!("invalid gate key format: {key}")))
            }
        } else if key.starts_with("published/") {
            // key format: published/L{level}/N{node_idx}
            // We map to SETNX published:L{level}:N{node_idx} 1
            let parts: Vec<&str> = key.split('/').collect();
            if parts.len() == 3 {
                let level_str = parts[1];
                let node_str = parts[2];
                let redis_key = format!("published:{}:{}", level_str, node_str);
                
                let set: bool = conn.set_nx(&redis_key, "1")
                    .map_err(|e| CasError(format!("redis set_nx: {e}")))?;
                
                if set {
                    let _: () = conn.expire(&redis_key, self.ttl_secs as i64)
                        .map_err(|e| CasError(format!("redis expire: {e}")))?;
                    Ok(CommitOutcome::Committed)
                } else {
                    Ok(CommitOutcome::AlreadyExists)
                }
            } else {
                Err(CasError(format!("invalid published key format: {key}")))
            }
        } else {
            let set: bool = conn.set_nx(key, bytes)
                .map_err(|e| CasError(format!("redis set_nx fallback: {e}")))?;
            if set {
                let _: () = conn.expire(key, self.ttl_secs as i64)
                    .map_err(|e| CasError(format!("redis expire fallback: {e}")))?;
                Ok(CommitOutcome::Committed)
            } else {
                Ok(CommitOutcome::AlreadyExists)
            }
        }
    }

    fn exists(&self, key: &str) -> Result<bool, CasError> {
        use redis::Commands as _;
        let mut conn = self.get_connection()?;
        
        if key.starts_with("gate/") {
             let parts: Vec<&str> = key.split('/').collect();
             if parts.len() == 4 && parts[3].starts_with("child_") {
                 let level_str = parts[1];
                 let node_str = parts[2];
                 let child_idx_str = &parts[3]["child_".len()..];
                 let redis_key = format!("gate:{}:{}", level_str, node_str);
                 let exists: bool = conn.sismember(&redis_key, child_idx_str)
                     .map_err(|e| CasError(format!("redis sismember: {e}")))?;
                 Ok(exists)
             } else {
                 Err(CasError(format!("invalid gate key format for exists: {key}")))
             }
        } else if key.starts_with("published/") {
             let parts: Vec<&str> = key.split('/').collect();
             if parts.len() == 3 {
                 let level_str = parts[1];
                 let node_str = parts[2];
                 let redis_key = format!("published:{}:{}", level_str, node_str);
                 let exists: bool = conn.exists(&redis_key)
                     .map_err(|e| CasError(format!("redis exists: {e}")))?;
                 Ok(exists)
             } else {
                 Err(CasError(format!("invalid published key format for exists: {key}")))
             }
        } else {
             let exists: bool = conn.exists(key)
                 .map_err(|e| CasError(format!("redis exists fallback: {e}")))?;
             Ok(exists)
        }
    }

    fn read(&self, _key: &str) -> Result<Option<Vec<u8>>, CasError> {
        Err(CasError("RedisCasStore::read not implemented".to_string()))
    }

    fn count_prefix(&self, prefix: &str) -> Result<usize, CasError> {
        use redis::Commands as _;
        let mut conn = self.get_connection()?;

        if prefix.starts_with("gate/") {
            // prefix format: gate/L{level}/N{node_idx}/child_
            // We map to SCARD gate:L{level}:N{node_idx}
            let parts: Vec<&str> = prefix.split('/').collect();
            if parts.len() == 4 && parts[3] == "child_" {
                let level_str = parts[1];
                let node_str = parts[2];
                let redis_key = format!("gate:{}:{}", level_str, node_str);
                let count: usize = conn.scard(&redis_key)
                    .map_err(|e| CasError(format!("redis scard: {e}")))?;
                Ok(count)
            } else {
                Err(CasError(format!("invalid gate prefix format: {prefix}")))
            }
        } else {
            Err(CasError(format!("RedisCasStore::count_prefix only supports gate/ prefixes, got: {prefix}")))
        }
    }
}

