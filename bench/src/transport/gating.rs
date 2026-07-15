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

/// The explicit, inspectable pairing of a same-height interval in the padded
/// perfect binary reduction tree (issue #321 Phase 4). Given any interval you
/// can print exactly who its partner is, whether it owns the merge, and the
/// single merged-parent interval the pair produces — the debuggability the
/// linked-list-style framing was chosen for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PairRole {
    /// `true` if this interval is the LEFT member and thus the OWNER of the pair
    /// (it names its required right sibling). `false` if it is the RIGHT member,
    /// owned by its predecessor.
    pub is_left_owner: bool,
    /// The fixed same-height sibling this interval pairs with: the right sibling
    /// if this is the left owner, else the left sibling. Fixed by POSITION, never
    /// "whichever neighbour commits first" — the property that makes
    /// double-consumption impossible.
    pub partner: (usize, usize),
    /// The single deterministic merged-parent interval `[left.lo, right.hi]` this
    /// pair produces, identical no matter which member computes it.
    pub merged: (usize, usize),
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
    // fixed `(level, node_idx)` node. The reduction path instead merges ADJACENT
    // same-height intervals opportunistically, the moment both members of a pair
    // are committed — so folding overlaps leaf proving and a straggler only
    // delays its own pair, not the whole level.
    //
    // PADDED PERFECT BINARY TREE (handles ANY leaf count, incl. non-powers-of-2).
    // We pad N leaves up to P = next_power_of_two(N). Real leaves are indices
    // [0, N); padding leaves are [N, P). This makes the tree a PERFECT binary
    // tree, so every interval has EXACTLY ONE same-height sibling at every level
    // — no "carry"/leftover element is ever stranded (the failure mode of strict
    // same-height pairing on an odd count). Padding always lands on the RIGHT
    // (high indices), so a fold whose right child is entirely padding is a no-op
    // passthrough of the left child (`right_is_real = false`, already supported by
    // `BinaryTreeChainCircuit`). Padding leaves need NO proof — they are treated
    // as already-present, so a right-padding fold fires as soon as its real left
    // child commits. For N=125, P=128 costs just 3 such no-op folds.
    //
    // EXPLICIT PAIRING (debuggable, linked-list-flavored). Every interval has an
    // inspectable role via `pair_role`: it is the LEFT owner of its pair (and
    // names its required right sibling), or the RIGHT member (owned by its
    // predecessor). The merged parent is always `[left.lo, right.hi]`, a single
    // deterministic identity per pair — so at any moment you can point at a proof
    // and say exactly who its partner is and who owns the merge.
    //
    // TWO correctness hazards, both defeated:
    // 1. DOUBLE-CONSUMPTION (silent-deadlock). An interval could be merged by only
    //    ONE pair because its partner is FIXED by position, never "whichever
    //    neighbour commits first". So no interval is consumed by two merges; no
    //    overlapping intervals; the tree always reaches the single root [0, P-1].
    // 2. EXACTLY-ONCE + RECOVERY. The merged parent publishes under a CAS marker
    //    keyed by the MERGED interval, so racing members publish once. The marker
    //    value is a lease stamp for Phase-5 crash-recovery re-drive (not a mere
    //    TTL-GC).
    // ─────────────────────────────────────────────────────────────────────

    /// Smallest power of two >= `n` (with `pad(0)=pad(1)=1`). The padded leaf
    /// count P: real leaves are `[0, n)`, padding leaves `[n, P)`.
    pub fn padded_leaf_count(n: usize) -> usize {
        if n == 0 {
            1
        } else {
            // Smallest power of two >= n: 2->2, 4->4, 5->8, 125->128, 500->512.
            n.next_power_of_two()
        }
    }

    /// Marker recording that the interval `[lo, hi]` has committed.
    fn interval_marker_key(lo: usize, hi: usize) -> String {
        format!("rgate/committed/{lo}_{hi}")
    }

    /// Publish-marker guaranteeing the merged parent `[lo, hi]` is published
    /// once. Its value is a lease stamp (millis) for crash-recovery re-drive.
    fn merge_publish_marker_key(lo: usize, hi: usize) -> String {
        format!("rgate/published/{lo}_{hi}")
    }

    /// The explicit pairing of a same-height interval `[lo, hi]` (span
    /// `2^level`) in the padded perfect binary tree.
    fn pair_role(lo: usize, hi: usize) -> PairRole {
        let span = hi - lo + 1;
        let block = lo / span; // index among same-height siblings at this level
        if block % 2 == 0 {
            // LEFT owner: its required partner is the right sibling.
            PairRole {
                is_left_owner: true,
                partner: (hi + 1, hi + span),
                merged: (lo, hi + span),
            }
        } else {
            // RIGHT member: owned by its predecessor (the left sibling).
            PairRole {
                is_left_owner: false,
                partner: (lo - span, lo - 1),
                merged: (lo - span, hi),
            }
        }
    }

    /// Whether interval `[lo, hi]` is ENTIRELY padding (`lo >= real_leaf_count`),
    /// i.e. it covers only padding leaves and therefore needs no proof — a fold
    /// with such a right child is a `right_is_real = false` no-op passthrough.
    fn is_all_padding(lo: usize, real_leaf_count: usize) -> bool {
        lo >= real_leaf_count
    }

    /// Advance order-free reduction gating for a just-committed reduction/leaf
    /// interval and, when its explicit same-height partner is present (or is
    /// entirely padding), publish the merged parent fold EXACTLY once.
    ///
    /// `leaf_count` is the REAL leaf count N; the tree is padded internally to
    /// `padded_leaf_count(N)`. `committed` must be the output-commit CAS outcome
    /// (only the output winner drives the gate). A leaf `i` is `[i, i]` at level
    /// 0; a reduction fold output is `[lo, hi]` at its level.
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

        let padded = Self::padded_leaf_count(leaf_count);

        // Record THIS interval as committed (idempotent via the marker name).
        let _ = self
            .store
            .cas_create(&Self::interval_marker_key(lo, hi), b"1")?;

        // The padded tree spans [0, padded-1]. If this interval already spans it,
        // it is the root — nothing above to merge.
        if lo == 0 && hi == padded - 1 {
            return Ok(GatingOutcome::RootReached);
        }

        let role = Self::pair_role(lo, hi);
        let (mlo, mhi) = role.merged;
        let merged_level = level + 1;
        let (plo, phi) = role.partner;

        // The partner is available if it is committed OR it is entirely padding
        // (padding needs no proof — it is virtually present from the start). A
        // padding partner is always the RIGHT sibling of a real left owner, so
        // this is exactly the `right_is_real = false` no-op fold.
        let partner_is_padding = Self::is_all_padding(plo, leaf_count);
        let partner_present = partner_is_padding
            || self
                .store
                .exists(&Self::interval_marker_key(plo, phi))?;
        if !partner_present {
            return Ok(GatingOutcome::Recorded { have: 1, needed: 2 });
        }

        // Both available → publish the merged parent EXACTLY once, guarded by the
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

    /// Drive a FULL reduction to the root for `n` real leaves, feeding each
    /// published merge output back in as its own commit (as the Phase-5
    /// coordinator will). Returns the set of distinct published fold keys and
    /// whether the padded root `[0, padded-1]` was reached. Leaves commit in the
    /// given `order` (indices into `0..n`) to exercise arrival-order independence.
    fn run_full_reduction(n: usize, order: &[usize]) -> (std::collections::HashSet<String>, bool) {
        type G<'a> = GatingEngine<'a, InMemoryCasStore, RecordingPublisher>;
        let store = InMemoryCasStore::new();
        let pubr = RecordingPublisher::new();
        let eng = GatingEngine::new(&store, &pubr);
        let padded = G::padded_leaf_count(n);

        // Queue of committed intervals to process; seed with the real leaves in
        // `order`. Each published merge is fed back in as a new commit.
        let mut queue: std::collections::VecDeque<(usize, usize, usize)> =
            order.iter().map(|&i| (i, i, 0usize)).collect();
        let mut root_reached = false;
        let mut guard = 0usize;
        while let Some((lo, hi, level)) = queue.pop_front() {
            guard += 1;
            assert!(guard < 100_000, "reduction did not terminate for n={n}");
            match eng
                .on_interval_committed(lo, hi, level, 2, n, 1, CommitOutcome::Committed)
                .unwrap()
            {
                GatingOutcome::PublishedParent(d) => {
                    if d.lo == 0 && d.hi == padded - 1 {
                        // Root fold published; committing it will report RootReached.
                        let r = eng
                            .on_interval_committed(d.lo, d.hi, d.level, 2, n, 1, CommitOutcome::Committed)
                            .unwrap();
                        if r == GatingOutcome::RootReached {
                            root_reached = true;
                        }
                    } else {
                        // Feed the merged output back in as a committed interval.
                        queue.push_back((d.lo, d.hi, d.level));
                    }
                }
                GatingOutcome::RootReached => root_reached = true,
                _ => {}
            }
        }
        (pubr.distinct_keys(), root_reached)
    }

    /// The merge fires the moment the SECOND member of a pair lands, regardless
    /// of arrival order, publishing the merged interval exactly once — both orders.
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

    /// THE double-consumption race guard. Adversarial commit order where a naive
    /// impl could merge the mis-aligned [1,2] pair; the deterministic pairing must
    /// yield only the valid [0,1] and [2,3] merges, never [1,2].
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

    /// Power-of-two count reaches the root (baseline).
    #[test]
    fn full_reduction_power_of_two_reaches_root() {
        for n in [2usize, 4, 8] {
            let order: Vec<usize> = (0..n).collect();
            let (_keys, reached) = run_full_reduction(n, &order);
            assert!(reached, "n={n} must reach the root");
        }
    }

    /// ODD / non-power-of-two counts reach the root via PADDING — the case the
    /// original strict same-height design STRANDED. N=5 pads to 8, N=125 pads to
    /// 128. This is the regression guard for the carry hole.
    #[test]
    fn full_reduction_odd_counts_reach_root_via_padding() {
        for n in [3usize, 5, 6, 7, 9, 125] {
            let order: Vec<usize> = (0..n).collect();
            let (keys, reached) = run_full_reduction(n, &order);
            assert!(
                reached,
                "n={n} (padded to {}) must reach the root — no stranded carry",
                GatingEngine::<InMemoryCasStore, RecordingPublisher>::padded_leaf_count(n)
            );
            // Sanity: at least one right-padding no-op fold exists for non-powers
            // of two (the merged interval extends past the last real leaf).
            let has_padding_fold = keys.iter().any(|k| {
                // reduction_{lo}_{hi}.proof with hi >= n indicates padding on the right.
                k.strip_prefix("reduction_")
                    .and_then(|r| r.strip_suffix(".proof"))
                    .and_then(|r| r.split_once('_'))
                    .and_then(|(_, hi)| hi.parse::<usize>().ok())
                    .map(|hi| hi >= n)
                    .unwrap_or(false)
            });
            if !n.is_power_of_two() {
                assert!(has_padding_fold, "n={n} must include a right-padding no-op fold");
            }
        }
    }

    /// Root is reached regardless of leaf ARRIVAL ORDER (reverse + a rotation),
    /// for both an even and an odd count.
    #[test]
    fn full_reduction_reaches_root_any_arrival_order() {
        for n in [4usize, 5, 8] {
            let mut rev: Vec<usize> = (0..n).collect();
            rev.reverse();
            let (_k, reached_rev) = run_full_reduction(n, &rev);
            assert!(reached_rev, "n={n} reversed order must reach root");

            let mut rot: Vec<usize> = (0..n).collect();
            rot.rotate_left(n / 2);
            let (_k2, reached_rot) = run_full_reduction(n, &rot);
            assert!(reached_rot, "n={n} rotated order must reach root");
        }
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

    /// `padded_leaf_count` and the explicit `pair_role` are correct + inspectable.
    #[test]
    fn padding_and_pair_role_are_correct() {
        type G<'a> = GatingEngine<'a, InMemoryCasStore, RecordingPublisher>;
        assert_eq!(G::padded_leaf_count(1), 1);
        assert_eq!(G::padded_leaf_count(2), 2);
        assert_eq!(G::padded_leaf_count(5), 8);
        assert_eq!(G::padded_leaf_count(125), 128);
        assert_eq!(G::padded_leaf_count(500), 512);

        // [0,0] is a LEFT owner; partner = right sibling [1,1]; merged [0,1].
        let r0 = G::pair_role(0, 0);
        assert!(r0.is_left_owner);
        assert_eq!(r0.partner, (1, 1));
        assert_eq!(r0.merged, (0, 1));
        // [1,1] is a RIGHT member; partner = left sibling [0,0]; merged [0,1].
        let r1 = G::pair_role(1, 1);
        assert!(!r1.is_left_owner);
        assert_eq!(r1.partner, (0, 0));
        assert_eq!(r1.merged, (0, 1));
        // [2,3] (level 1) is a RIGHT member; partner [0,1]; merged [0,3].
        let r23 = G::pair_role(2, 3);
        assert!(!r23.is_left_owner);
        assert_eq!(r23.partner, (0, 1));
        assert_eq!(r23.merged, (0, 3));

        // is_all_padding: for N=5, leaf [5,5] is padding, [4,4] is real.
        assert!(G::is_all_padding(5, 5));
        assert!(!G::is_all_padding(4, 5));
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

