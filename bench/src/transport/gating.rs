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

    /// (#321 Phase 5) Unconditional overwrite (SET, last-writer-wins) — the
    /// non-CAS write primitive crash-recovery ([`GatingEngine::redrive_stale_merges`])
    /// needs to REFRESH a stale lease stamp on an existing publish marker (the
    /// CAS `cas_create` deliberately WON'T overwrite an existing key). Distinct
    /// from `cas_create`: this is intentionally NOT exactly-one-winner and MUST
    /// only be used where last-writer-wins is correct (a lease-stamp refresh,
    /// which is idempotent-in-effect: the value is a monotone-ish timestamp).
    ///
    /// Default implementation returns `Err` ("unsupported") so a backend that
    /// cannot overwrite is honest rather than silently wrong; recovery then
    /// degrades to the ABSENT-marker-only re-drive (see `redrive_stale_merges`).
    fn cas_put(&self, _key: &str, _bytes: &[u8]) -> Result<(), CasError> {
        Err(CasError(
            "cas_put (overwrite) not supported by this CasStore".to_string(),
        ))
    }
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

/// Smallest power of two >= `n` (with `pad(0)=pad(1)=1`). The padded leaf count
/// P for the padded perfect binary reduction tree: real leaves are `[0, n)`,
/// padding leaves `[n, P)`. Free function so both the generic
/// [`GatingEngine::padded_leaf_count`] AND non-generic callers
/// ([`crate::transport::reduction_root_key`]) share ONE definition of padding —
/// the root interval `[0, P-1]` (which `on_interval_committed` treats as
/// `RootReached`) is thus computed identically everywhere.
pub fn padded_leaf_count(n: usize) -> usize {
    if n == 0 {
        1
    } else {
        // Smallest power of two >= n: 2->2, 4->4, 5->8, 125->128, 500->512.
        n.next_power_of_two()
    }
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
    /// count P: real leaves are `[0, n)`, padding leaves `[n, P)`. Delegates to
    /// the free [`padded_leaf_count`] so non-generic callers (e.g.
    /// [`crate::transport::reduction_root_key`]) can reuse the SAME padding logic
    /// without naming the generic `GatingEngine` type parameters.
    pub fn padded_leaf_count(n: usize) -> usize {
        padded_leaf_count(n)
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
        let lease_stamp = Self::now_lease_stamp_ms().to_string();
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

    /// Current wall-clock lease stamp in milliseconds since the UNIX epoch, as a
    /// string (the VALUE stored under a merge publish marker). Factored out so the
    /// normal-gate publish and the recovery re-drive stamp identically.
    fn now_lease_stamp_ms() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    }

    // ─────────────────────────────────────────────────────────────────────
    // Lease-based crash recovery — issue #321 Phase 5
    //
    // THE HAZARD: a merge can be lost forever. The pair-owner won the publish-
    // marker CAS (so no other member will ever re-publish — exactly-once is doing
    // its job), then crashed BEFORE the merged fold descriptor was durably
    // enqueued (or the enqueue was dropped). The seam is now "stuck": both
    // members are committed, the publish marker exists, yet no fold task is in the
    // queue. Nothing re-drives it. This is the "TTL-is-GC-not-lease" trap — a TTL
    // would eventually delete the marker and let a DIFFERENT member re-publish,
    // but only members that re-commit, and only after a GC delay; it is not a
    // lease you can deterministically reclaim.
    //
    // THE FIX: the publish marker's VALUE is a LEASE STAMP (millis). A separate
    // recovery pass (this method) treats a publish marker whose stamp is older
    // than `lease_timeout_ms` as an EXPIRED LEASE and RE-PUBLISHES the exact same
    // merged fold descriptor (same deterministic `pair_role` identity — never a
    // new/overlapping interval), refreshing the stamp. A marker that is ABSENT
    // (owner crashed before even the CAS) is also re-driven. This is a pure
    // `GatingEngine` method so it is unit-testable against `InMemoryCasStore`
    // with NO coordinator / Pub/Sub.
    // ─────────────────────────────────────────────────────────────────────

    /// Scan the padded reduction tree for merges whose BOTH members are committed
    /// but whose merged fold was never (or may never be) durably enqueued, and
    /// RE-DRIVE them: re-publish the exact same merged fold descriptor. Returns
    /// the number of merges re-driven.
    ///
    /// A merged interval `[mlo, mhi]` is re-driven when both members of its pair
    /// are present (committed, or the right member is entirely padding) AND its
    /// publish marker `rgate/published/{mlo}_{mhi}` is either:
    ///   * **absent** — the owner crashed before winning the publish CAS; OR
    ///   * **present but STALE** — its lease-stamp value is older than
    ///     `now - lease_timeout_ms` (the owner won the CAS then crashed before the
    ///     fold was enqueued). The stamp is refreshed (via [`CasStore::cas_put`])
    ///     so a subsequent recovery pass does not re-drive it again until the new
    ///     lease also expires.
    ///
    /// If the store does not support overwrite ([`CasStore::cas_put`] returns
    /// `Err`), the stale-lease branch degrades HONESTLY to a no-op (it cannot
    /// refresh the stamp, so it does not re-drive to avoid a re-drive storm), and
    /// only the ABSENT-marker case is recovered. The `InMemoryCasStore` and
    /// `RedisCasStore` both support `cas_put`, so full stale-lease recovery is
    /// available in tests and production.
    ///
    /// Deterministic identity: the re-published descriptor uses the SAME
    /// `pair_role`/merged-interval identity as [`on_interval_committed`], so a
    /// re-drive targets EXACTLY the same merged interval — never a new or
    /// overlapping one (the padded-tree invariants are preserved).
    ///
    /// Re-drive is exactly-once per absent marker: when the marker is absent this
    /// method CAS-creates it (winning the same publish CAS the normal gate would),
    /// so a second recovery pass finds the marker present-and-fresh and does
    /// nothing.
    pub fn redrive_stale_merges(
        &self,
        leaf_count: usize,
        radix: usize,
        tx_per_proof: usize,
        lease_timeout_ms: u128,
    ) -> Result<usize, CasError> {
        let padded = Self::padded_leaf_count(leaf_count);
        let now = Self::now_lease_stamp_ms();
        let mut redriven = 0usize;

        // Walk the padded perfect binary tree level by level. At `level` the
        // same-height intervals have span `2^level`; a merged parent at
        // `level+1` is produced by the LEFT owner of each pair. We enumerate
        // every LEFT-owner interval `[lo, hi]` at each level (lo advancing by
        // 2*span so we only visit owners), and consider its merge `[lo, hi+span]`.
        let mut span = 1usize; // 2^level, starting at level 0 (single leaves)
        let mut level = 0usize;
        while span < padded {
            let merged_span = span * 2;
            let mut lo = 0usize;
            while lo < padded {
                let hi = lo + span - 1;
                // Only left owners (block index even) produce a merge here; the
                // step of 2*span already visits exactly the left owners.
                let role = Self::pair_role(lo, hi);
                debug_assert!(
                    role.is_left_owner,
                    "level {level}: [{lo},{hi}] should be a left owner"
                );
                let (mlo, mhi) = role.merged; // = (lo, hi + span)
                let (plo, phi) = role.partner; // right sibling = (hi+1, hi+span)

                // Both members present? Left owner committed AND (partner
                // committed OR partner entirely padding). Same availability test
                // as `on_interval_committed`.
                let left_present = self
                    .store
                    .exists(&Self::interval_marker_key(lo, hi))?;
                let partner_present = Self::is_all_padding(plo, leaf_count)
                    || self.store.exists(&Self::interval_marker_key(plo, phi))?;

                if left_present && partner_present {
                    let pub_marker = Self::merge_publish_marker_key(mlo, mhi);
                    let existing = self.store.read(&pub_marker)?;
                    let should_redrive = match &existing {
                        // Owner crashed before winning the publish CAS: re-drive.
                        None => true,
                        // Marker present: re-drive only if its lease stamp is STALE
                        // (older than now - lease_timeout_ms). A fresh stamp means
                        // the merge was published recently; leave it alone.
                        Some(val) => {
                            let stamp = std::str::from_utf8(val)
                                .ok()
                                .and_then(|s| s.trim().parse::<u128>().ok())
                                .unwrap_or(0);
                            now.saturating_sub(stamp) > lease_timeout_ms
                        }
                    };

                    if should_redrive {
                        let stamp = now.to_string();
                        // Claim/refresh the lease. Absent -> CAS-create (exactly
                        // one recovery pass wins). Present-but-stale -> overwrite
                        // via cas_put; if unsupported, degrade honestly.
                        let claimed = match &existing {
                            None => {
                                matches!(
                                    self.store.cas_create(&pub_marker, stamp.as_bytes())?,
                                    CommitOutcome::Committed
                                )
                            }
                            Some(_) => match self.store.cas_put(&pub_marker, stamp.as_bytes()) {
                                Ok(()) => true,
                                // Overwrite unsupported: cannot refresh the stale
                                // lease safely, so do NOT re-drive (honest degrade,
                                // documented). ABSENT-marker recovery still works.
                                Err(_) => false,
                            },
                        };
                        if claimed {
                            let merged_level = level + 1;
                            let mut fold = WorkDescriptor::reduction_fold(
                                mlo,
                                mhi,
                                merged_level,
                                radix,
                                leaf_count,
                                tx_per_proof,
                            );
                            // Flag the re-driven descriptor so the completion event
                            // can surface `redriven_after_lease_expiry` (#321 P5 /
                            // #328 telemetry).
                            fold.redriven = true;
                            self.publisher.publish(fold)?;
                            redriven += 1;
                        }
                    }
                }

                lo += merged_span;
            }
            span = merged_span;
            level += 1;
        }

        Ok(redriven)
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

    fn cas_put(&self, key: &str, bytes: &[u8]) -> Result<(), CasError> {
        // Unconditional last-writer-wins overwrite (models GCS upload with no
        // ifGenerationMatch precondition, or a Redis SET).
        self.inner
            .lock()
            .expect("cas mutex poisoned")
            .insert(key.to_string(), bytes.to_vec());
        Ok(())
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

    /// Padding tracks the CHUNK SIZE C, because the real leaf count is
    /// `N = ceil(txs_per_block / C)` — it is NOT a hardcoded number. Same 500-tx
    /// block at different C yields different N, each padded to its own P and each
    /// reaching the root. Guards against ever baking a fixed padding boundary.
    #[test]
    fn padding_tracks_chunk_size_derived_leaf_count() {
        type G<'a> = GatingEngine<'a, InMemoryCasStore, RecordingPublisher>;
        let txs_per_block = 500usize;
        // (C, expected N = ceil(500/C), expected padded P)
        let cases = [
            (1usize, 500usize, 512usize),
            (2, 250, 256),
            (4, 125, 128),
            (8, 63, 64),
        ];
        for (c, expected_n, expected_p) in cases {
            let n = txs_per_block.div_ceil(c);
            assert_eq!(n, expected_n, "C={c}: N must be ceil(500/C)");
            assert_eq!(
                G::padded_leaf_count(n),
                expected_p,
                "C={c}: padded leaf count must track N (never hardcoded)"
            );
            // And the reduction actually reaches the root at this C-derived N.
            let order: Vec<usize> = (0..n).collect();
            let (_keys, reached) = run_full_reduction(n, &order);
            assert!(reached, "C={c} (N={n}, P={expected_p}) must reach the root");
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

    // ── Lease-based crash recovery re-drive — #321 Phase 5 ───────────────────

    /// THE recovery test: both members of a pair are committed but the merge was
    /// never published (the publish marker is ABSENT — the owner crashed before
    /// winning the CAS). `redrive_stale_merges` must re-publish EXACTLY the
    /// correct merged interval, EXACTLY once (a second call is a no-op because the
    /// marker now exists and is fresh). This proves the anti-"seam stuck forever"
    /// recovery path.
    #[test]
    fn redrive_stale_merges_republishes_lost_merge_exactly_once() {
        type G<'a> = GatingEngine<'a, InMemoryCasStore, RecordingPublisher>;
        let store = InMemoryCasStore::new();
        let pubr = RecordingPublisher::new();
        let eng = GatingEngine::new(&store, &pubr);

        // Simulate the lost merge for N=2, radix=2: BOTH leaf intervals [0,0] and
        // [1,1] committed, but the merged [0,1] fold was NEVER published (its
        // publish marker rgate/published/0_1 is ABSENT). We create ONLY the two
        // committed markers — not the publish marker — so the seam is "stuck".
        store.cas_create(&G::interval_marker_key(0, 0), b"1").unwrap();
        store.cas_create(&G::interval_marker_key(1, 1), b"1").unwrap();
        assert!(
            pubr.published().is_empty(),
            "no merge published yet (seam is stuck)"
        );

        // Recovery pass re-drives the lost merge exactly once.
        let n = eng.redrive_stale_merges(2, 2, 1, 60_000).unwrap();
        assert_eq!(n, 1, "exactly one merge re-driven");
        let published = pubr.published();
        assert_eq!(published.len(), 1, "exactly one descriptor re-published");
        let d = &published[0];
        assert_eq!(d.output_key(), "reduction_0_1.proof", "correct merged interval");
        assert_eq!(d.lo, 0);
        assert_eq!(d.hi, 1);
        assert_eq!(d.level, 1);
        assert!(
            d.redriven,
            "re-driven descriptor must be flagged for redriven_after_lease_expiry"
        );

        // A SECOND recovery pass is a no-op: the marker now exists and is fresh
        // (its lease stamp is recent, well within the 60s timeout).
        let n2 = eng.redrive_stale_merges(2, 2, 1, 60_000).unwrap();
        assert_eq!(n2, 0, "second pass must not re-drive (marker present + fresh)");
        assert_eq!(pubr.published().len(), 1, "still exactly one descriptor");
    }

    /// A publish marker that EXISTS but is STALE (lease stamp older than the
    /// timeout) is re-driven — the owner won the CAS then crashed before the fold
    /// was enqueued. With `cas_put` supported (InMemory), the stamp is refreshed
    /// so a subsequent pass does not re-drive again.
    #[test]
    fn redrive_stale_merges_republishes_on_expired_lease() {
        type G<'a> = GatingEngine<'a, InMemoryCasStore, RecordingPublisher>;
        let store = InMemoryCasStore::new();
        let pubr = RecordingPublisher::new();
        let eng = GatingEngine::new(&store, &pubr);

        store.cas_create(&G::interval_marker_key(0, 0), b"1").unwrap();
        store.cas_create(&G::interval_marker_key(1, 1), b"1").unwrap();
        // Pre-create the publish marker with an ANCIENT lease stamp (millis=1),
        // i.e. the owner claimed it long ago then crashed before enqueue.
        store
            .cas_create(&G::merge_publish_marker_key(0, 1), b"1")
            .unwrap();

        // With a tiny timeout, the stamp (1ms) is far older than now-timeout, so
        // it is stale and re-driven.
        let n = eng.redrive_stale_merges(2, 2, 1, 1_000).unwrap();
        assert_eq!(n, 1, "stale-lease merge must be re-driven");
        assert_eq!(pubr.published().len(), 1);
        assert!(pubr.published()[0].redriven);

        // The stamp was refreshed (fresh now); a second pass does not re-drive.
        let n2 = eng.redrive_stale_merges(2, 2, 1, 1_000).unwrap();
        assert_eq!(n2, 0, "refreshed lease must not be re-driven again");
    }

    /// A merge whose BOTH members are present AND whose publish marker is FRESH
    /// is NOT re-driven (normal healthy state — recovery must not double-publish).
    #[test]
    fn redrive_stale_merges_ignores_fresh_published_merge() {
        let store = InMemoryCasStore::new();
        let pubr = RecordingPublisher::new();
        let eng = GatingEngine::new(&store, &pubr);

        // Drive the pair normally so the merge is published with a fresh stamp.
        eng.on_interval_committed(0, 0, 0, 2, 2, 1, CommitOutcome::Committed)
            .unwrap();
        let o = eng
            .on_interval_committed(1, 1, 0, 2, 2, 1, CommitOutcome::Committed)
            .unwrap();
        assert!(matches!(o, GatingOutcome::PublishedParent(_)));
        assert_eq!(pubr.published().len(), 1);

        // Recovery finds a fresh publish marker => no re-drive.
        let n = eng.redrive_stale_merges(2, 2, 1, 60_000).unwrap();
        assert_eq!(n, 0, "a freshly-published merge must not be re-driven");
        assert_eq!(pubr.published().len(), 1, "still exactly one publish");
    }

    /// Recovery does nothing when a pair is INCOMPLETE (only one member
    /// committed) — there is no merge to re-drive yet.
    #[test]
    fn redrive_stale_merges_skips_incomplete_pairs() {
        type G<'a> = GatingEngine<'a, InMemoryCasStore, RecordingPublisher>;
        let store = InMemoryCasStore::new();
        let pubr = RecordingPublisher::new();
        let eng = GatingEngine::new(&store, &pubr);
        // Only the left member committed; the partner [1,1] is absent.
        store.cas_create(&G::interval_marker_key(0, 0), b"1").unwrap();
        let n = eng.redrive_stale_merges(2, 2, 1, 60_000).unwrap();
        assert_eq!(n, 0, "incomplete pair must not be re-driven");
        assert!(pubr.published().is_empty());
    }

    /// `cas_put` on the in-memory double is a last-writer-wins overwrite (the
    /// primitive stale-lease refresh needs), distinct from the exactly-one-winner
    /// `cas_create`.
    #[test]
    fn cas_put_overwrites_last_writer_wins() {
        let s = InMemoryCasStore::new();
        assert_eq!(s.cas_create("k", b"first").unwrap(), CommitOutcome::Committed);
        // cas_create refuses to overwrite (exactly-one-winner).
        assert_eq!(
            s.cas_create("k", b"second").unwrap(),
            CommitOutcome::AlreadyExists
        );
        assert_eq!(s.read("k").unwrap().unwrap(), b"first");
        // cas_put overwrites unconditionally.
        s.cas_put("k", b"third").unwrap();
        assert_eq!(s.read("k").unwrap().unwrap(), b"third");
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

    fn cas_put(&self, key: &str, bytes: &[u8]) -> Result<(), CasError> {
        // (#321 Phase 5) Unconditional SET (last-writer-wins) for lease-stamp
        // refresh during crash-recovery re-drive. Only the interval publish
        // markers (`rgate/published/{lo}_{hi}`) are refreshed this way; they are
        // stored as plain string keys (unlike the SADD/SETNX-mapped gate/published
        // keys), so a straight SET + expire is correct.
        use redis::Commands as _;
        let mut conn = self.get_connection()?;
        let _: () = conn
            .set(key, bytes)
            .map_err(|e| CasError(format!("redis set (cas_put): {e}")))?;
        let _: () = conn
            .expire(key, self.ttl_secs as i64)
            .map_err(|e| CasError(format!("redis expire (cas_put): {e}")))?;
        Ok(())
    }
}

