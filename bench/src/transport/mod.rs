// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! Work-transport abstraction for the fungible recursive-proving worker pool.
//!
//! This module defines the **durable contract** between the prover-node binary
//! and whatever queue/object-store backs it: a [`WorkTransport`] from which a
//! fungible worker *pulls* a [`WorkDescriptor`], assumes the role it names,
//! executes it, *commits* the resulting proof bytes idempotently, and *acks*.
//! The trait is the artifact that outlives any single backend; the
//! [`LocalTransport`] in this module is an in-process / filesystem
//! implementation used for development and tests (no broker, no cloud). The
//! production Pub/Sub + GCS-native backend is a separate follow-up that
//! implements the **same trait**.
//!
//! # Why these specific operations exist (verified primitives)
//!
//! The trait surface is shaped by two empirically-verified primitives from a
//! pilot; getting them wrong corrupts a distributed run, so they are baked into
//! the contract rather than left to each backend's discretion:
//!
//! 1. **Idempotent output / atomic claim is an atomic compare-and-swap
//!    create-if-absent** — [`WorkTransport::commit_output`]. Two pods may, by
//!    design (Spot preemption, redelivery), execute the *same* descriptor; the
//!    output store must admit **exactly one** writer per output key so the proof
//!    bytes are never half-written or interleaved. The pilot verified that:
//!      * GCS **native** API `ifGenerationMatch=0` is exactly-one-winner. ✅
//!      * GCS-**Fuse** `O_EXCL` is **REFUTED**: gcsfuse implements create as a
//!        non-atomic stat-then-create, so two pods on different nodes both
//!        "win" `O_EXCL` and corrupt the object. ❌
//!    Therefore a production backend MUST implement [`commit_output`] via the
//!    native object-store CAS (`ifGenerationMatch=0`), **never** a plain
//!    `OpenOptions::create_new`/`O_EXCL` on a shared or Fuse-backed mount. See
//!    the contract note on [`WorkTransport::commit_output`].
//!
//! 2. **Pull + lease-extend-while-working + ack-after-commit + nack-on-failure**
//!    is the verified Pub/Sub consumption pattern. The trait models it as
//!    [`WorkTransport::pull_one`] (flow control = 1 outstanding message),
//!    [`WorkLease::extend`] (heartbeat the ack deadline while proving),
//!    [`WorkLease::ack`] (only **after** the result is durably committed), and
//!    [`WorkLease::nack`] (abandon on failure so the message is redelivered).
//!
//! [`commit_output`]: WorkTransport::commit_output

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::telemetry::TaskTelemetry;

/// Backend-agnostic, cloud-free CAS object-store abstraction + readiness-gating
/// engine. This is the durable, unit-testable core that BOTH the [`LocalTransport`]
/// (filesystem CAS) and the production [`pubsub::PubSubGcsTransport`] (GCS native
/// `ifGenerationMatch=0` CAS) drive, so the "publish each fold descriptor exactly
/// once" invariant is implemented once and tested against an in-memory CAS double
/// with no network.
pub mod gating;

/// Production work-transport backend: GCP Pub/Sub (pull) + GCS native-API atomic
/// claim/commit. Compiled **only** under `--features pubsub`; the default build
/// stays cloud-free.
#[cfg(feature = "pubsub")]
pub mod pubsub;

// ─────────────────────────────────────────────────────────────────────────
// Work descriptors
// ─────────────────────────────────────────────────────────────────────────

/// The role a fungible worker assumes for a single message. Chosen
/// **per-message at runtime** — not baked into the deploy-time command — so one
/// `prover-node` image can be any role depending on what it pulls.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// Prove one transaction chunk into a level-0 leaf proof.
    Leaf,
    /// Fold the `radix` children of one node at `level` into a parent proof.
    TreeNode,
    /// (#321 Phase 3) Fold two ADJACENT same-height children spanning the leaf
    /// interval `[lo, hi]` into a parent covering that interval. The order-free
    /// reduction analogue of `TreeNode`: addressed by interval, not by a fixed
    /// `(level, node_idx)` slot. `level` still records the reduction height for
    /// telemetry + circuit selection.
    ReductionFold,
    /// Harvest and verify the single root proof.
    RootCoordinator,
}

impl Role {
    /// Stable string tag used in queue payloads and telemetry.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Leaf => "leaf",
            Role::TreeNode => "tree-node",
            Role::ReductionFold => "reduction-fold",
            Role::RootCoordinator => "root-coordinator",
        }
    }
}

/// A unit of work pulled from the transport. Carries the **role**, the
/// **geometry params** needed to locate the work in the tree, and **pointers**
/// (output keys) into the proof store — never proof bytes themselves. Inputs are
/// addressed by the same key scheme the geometry implies (a tree-node reads its
/// children's output keys), so a descriptor stays small and the heavy proof
/// payloads travel through the object store, not the queue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkDescriptor {
    /// The role to assume for this message.
    pub role: Role,
    /// Tree fan-in (children per node). The circuit is radix-shaped; under-full
    /// nodes are padded. radix=2 ⇒ depth = ceil(log2(N)).
    pub radix: usize,
    /// Total number of level-0 leaves N feeding the tree. Determines per-level
    /// node counts and overall depth.
    pub leaf_count: usize,
    /// Transactions per leaf proof (leaf role only; carried for all so the
    /// dispatcher can rebuild circuits deterministically).
    pub tx_per_proof: usize,
    /// Leaf role: the chunk index to prove. Ignored by other roles.
    pub chunk_idx: usize,
    /// Tree-node role: the level (>= 1) being folded. Ignored by leaf.
    pub level: usize,
    /// Tree-node role: the node index within `level`. Ignored by leaf.
    pub node_idx: usize,
    /// (#321 Phase 3) ReductionFold role: inclusive LEAF-index interval `[lo, hi]`
    /// this fold's OUTPUT covers. `#[serde(default)]` so descriptors serialized
    /// before Phase 3 (which lack these fields) still deserialize — for those the
    /// interval is `[0, 0]` and is simply unused by the non-reduction roles.
    #[serde(default)]
    pub lo: usize,
    /// Inclusive upper leaf index of the interval this fold's output covers.
    #[serde(default)]
    pub hi: usize,
}

impl WorkDescriptor {
    /// A leaf descriptor for chunk `chunk_idx`.
    pub fn leaf(chunk_idx: usize, radix: usize, leaf_count: usize, tx_per_proof: usize) -> Self {
        Self {
            role: Role::Leaf,
            radix,
            leaf_count,
            tx_per_proof,
            chunk_idx,
            level: 0,
            node_idx: 0,
            lo: 0,
            hi: 0,
        }
    }

    /// A tree-node fold descriptor for node `node_idx` at `level`.
    pub fn tree_node(
        level: usize,
        node_idx: usize,
        radix: usize,
        leaf_count: usize,
        tx_per_proof: usize,
    ) -> Self {
        Self {
            role: Role::TreeNode,
            radix,
            leaf_count,
            tx_per_proof,
            chunk_idx: 0,
            level,
            node_idx,
            lo: 0,
            hi: 0,
        }
    }

    /// (#321 Phase 3) A reduction-fold descriptor: fold the two adjacent
    /// same-height children whose combined output spans leaf interval `[lo, hi]`,
    /// at reduction `level`. Interval-addressed rather than `(level, node_idx)`.
    pub fn reduction_fold(
        lo: usize,
        hi: usize,
        level: usize,
        radix: usize,
        leaf_count: usize,
        tx_per_proof: usize,
    ) -> Self {
        Self {
            role: Role::ReductionFold,
            radix,
            leaf_count,
            tx_per_proof,
            chunk_idx: 0,
            level,
            node_idx: 0,
            lo,
            hi,
        }
    }

    /// A root-coordinator descriptor.
    pub fn root(radix: usize, leaf_count: usize, tx_per_proof: usize) -> Self {
        Self {
            role: Role::RootCoordinator,
            radix,
            leaf_count,
            tx_per_proof,
            chunk_idx: 0,
            level: 0,
            node_idx: 0,
            lo: 0,
            hi: 0,
        }
    }

    /// The object-store key this descriptor's output is committed under. This is
    /// the *pointer* a downstream descriptor reads from. Mirrors the filesystem
    /// proof-transport naming (`leaf_{i}.proof`, `tree_L{L}_N{n}.proof`) so the
    /// existing role code and the transport agree on locations.
    pub fn output_key(&self) -> String {
        match self.role {
            Role::Leaf => format!("leaf_{}.proof", self.chunk_idx),
            Role::TreeNode => format!("tree_L{}_N{}.proof", self.level, self.node_idx),
            // (#321 Phase 3) Interval-addressed output: any pod can name the
            // proof covering leaf interval [lo, hi] unambiguously, independent of
            // a radix-node slot. Enables opportunistic adjacent-pair merging.
            Role::ReductionFold => format!("reduction_{}_{}.proof", self.lo, self.hi),
            // The root coordinator produces no new proof; it verifies the
            // existing top node. Use a sentinel completion marker key.
            Role::RootCoordinator => "root_verified.marker".to_string(),
        }
    }
}

/// Event published by workers upon successful completion of a task.
/// Listened to by the external coordinator to drive the gating logic.
///
/// # Per-task telemetry (issue #328 Phase 1)
///
/// The fields below the original five enrich the completion payload with the
/// application-derived facts a benchmark run needs to derive resource sizing
/// WITHOUT GCP node metrics: peak memory, pre-state provenance, phase timers,
/// cold/warm separation, and the descriptor geometry that makes a report
/// self-describing. Every added field is `#[serde(default)]` so a pre-#328
/// payload (only the original five fields) still deserializes — wire back-compat,
/// exactly like the Phase-3 `WorkDescriptor::{lo,hi}` addition.
///
/// # Anti-fabrication
///
/// An unavailable metric is `0` (numeric) or `"n/a"` (string) — never a made-up
/// number. See `crate::telemetry` and `reports/PROVENANCE.md`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProverEvent {
    pub descriptor: WorkDescriptor,
    pub status: String, // "success" or "failed"
    pub prove_time_ms: u64,
    pub gcs_time_ms: u64,
    pub total_time_ms: u64,

    // ── #328 Phase 1: per-task telemetry (all serde(default) for back-compat) ──
    /// Peak resident memory for the task/pod, in bytes. `0` if no cgroup/proc
    /// source was readable (honest zero — never fabricated). See
    /// [`crate::telemetry::read_peak_rss_bytes`].
    #[serde(default)]
    pub peak_rss_bytes: u64,
    /// Pre-state provenance: `"corpus"` | `"replay-fallback"` | `"n/a"`
    /// (folds/root have no pre-state). Surfaced from the leaf path's
    /// `PreStateSource`. Defaults to `"n/a"` for pre-#328 payloads.
    #[serde(default = "prestate_source_default")]
    pub prestate_source: String,
    /// Phase timer: time to pull the message off the queue, in ms (best-effort).
    #[serde(default)]
    pub pull_ms: u64,
    /// Phase timer: pre-execution / setup time NOT part of the prove, in ms.
    /// Emitted `0` when not separately isolatable in the current plumbing.
    #[serde(default)]
    pub pre_exec_ms: u64,
    /// Phase timer: prove time, in ms. Duplicates `prove_time_ms` under the
    /// uniform phase-timer naming so a report can iterate phases uniformly.
    #[serde(default)]
    pub prove_ms: u64,
    /// Phase timer: object-store (GCS) write time, in ms. Duplicates
    /// `gcs_time_ms` under the uniform phase-timer naming.
    #[serde(default)]
    pub gcs_write_ms: u64,
    /// Phase timer: time the task waited in the queue before pull, in ms.
    /// Emitted `0` when not separately measurable with the current plumbing.
    #[serde(default)]
    pub queue_wait_ms: u64,
    /// `true` when this was the FIRST task executed on the pod (cold,
    /// circuit-build-paying); `false` for every subsequent task (warm, cached
    /// circuits). Separates cold vs cached folds (#322).
    #[serde(default)]
    pub is_first_task_on_pod: bool,
    /// Echo of `descriptor.tx_per_proof` — transactions per leaf proof — so a
    /// report self-describes without cross-referencing the descriptor.
    #[serde(default)]
    pub chunk_size: usize,
    /// Echo of `descriptor.leaf_count` — total leaves feeding the tree.
    #[serde(default)]
    pub leaf_count: usize,
}

/// serde default for [`ProverEvent::prestate_source`]: `"n/a"` so a pre-#328
/// payload (which omits the field) deserializes to the fold/root sentinel rather
/// than an empty string.
fn prestate_source_default() -> String {
    "n/a".to_string()
}

// ─────────────────────────────────────────────────────────────────────────
// Commit outcome
// ─────────────────────────────────────────────────────────────────────────

/// Result of an atomic [`WorkTransport::commit_output`]. Exactly one concurrent
/// caller for a given key observes [`Committed`]; every other observes
/// [`AlreadyExists`]. This is the idempotent-output guard: a redelivered or
/// duplicated descriptor that re-proves the same work commits the same key and
/// is harmlessly told the output already exists.
///
/// [`Committed`]: CommitOutcome::Committed
/// [`AlreadyExists`]: CommitOutcome::AlreadyExists
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitOutcome {
    /// This caller won the CAS create and wrote the bytes.
    Committed,
    /// The key already existed; this caller wrote nothing.
    AlreadyExists,
}

// ─────────────────────────────────────────────────────────────────────────
// Transport trait
// ─────────────────────────────────────────────────────────────────────────

/// The pull-based work transport a fungible worker consumes.
///
/// A worker loop is: `pull_one()` → assume the role → (periodically `extend()`)
/// → do the work → `commit_output()` → on `Committed` (or `AlreadyExists`)
/// `ack()`; on any failure `nack()`. The trait is deliberately minimal and
/// backend-agnostic so the production Pub/Sub + GCS backend implements the same
/// methods with native primitives.
pub trait WorkTransport: Send + Sync {
    /// The lease type this transport hands out for a pulled message.
    type Lease: WorkLease;

    /// Pull at most **one** outstanding message (flow control = 1). Returns
    /// `None` when the queue is empty. A real backend sets `maxMessages=1` and
    /// relies on the ack deadline + [`WorkLease::extend`] for at-least-once
    /// delivery; the local backend models a visibility timeout in-process.
    fn pull_one(&self) -> Option<Self::Lease>;

    /// Enqueue a follow-on descriptor (e.g. a leaf worker that completes the
    /// last child of a node publishes that node's fold task). Idempotency of
    /// *output* is guaranteed by [`commit_output`](Self::commit_output);
    /// duplicate *publishes* are tolerated because re-executing a descriptor
    /// whose output already exists is a no-op commit.
    fn publish(&self, descriptor: WorkDescriptor);

    /// Atomically commit `bytes` under `key` **if and only if** `key` does not
    /// already exist — an exactly-one-winner compare-and-swap create.
    ///
    /// # Contract for production backends (do not get this wrong)
    ///
    /// A production object-store backend **MUST** implement this via the native
    /// CAS create primitive — for GCS that is an upload with
    /// **`ifGenerationMatch=0`** (precondition: object generation 0 ⇒ does not
    /// exist). It **MUST NOT** implement it as a plain create / `O_EXCL` open on
    /// a shared or GCS-**Fuse**-backed mount: the pilot REFUTED gcsfuse
    /// `O_EXCL`, which performs a non-atomic stat-then-create and lets two pods
    /// on different nodes both "win", corrupting the object. The
    /// [`LocalTransport`] in this crate uses filesystem `O_EXCL`, which is
    /// atomic **only** on a single local filesystem and is therefore acceptable
    /// for single-node dev/test, never for the cross-node production path.
    fn commit_output(&self, key: &str, bytes: &[u8]) -> CommitOutcome;

    /// Whether `key` has already been committed. Used by the dispatch loop to
    /// detect completion / skip already-done work.
    fn output_exists(&self, key: &str) -> bool;

    /// Read previously committed bytes for `key`, if present.
    fn read_output(&self, key: &str) -> Option<Vec<u8>>;

    /// Commit a child's output **and** advance readiness gating in one call:
    /// commits `bytes` under `descriptor.output_key()` via the atomic
    /// [`commit_output`](Self::commit_output) CAS, then (only if this caller won
    /// the CAS) publishes the parent fold descriptor exactly once when the
    /// parent's real-child quota is met. This is the single primitive the
    /// fungible dispatch loop uses, so a generic `run_dispatch_loop<T:
    /// WorkTransport>` drives ANY backend (the [`LocalTransport`] filesystem CAS
    /// or the production GCS-native CAS) through the same trait method.
    ///
    /// Each backend implements this with its native gating engine (filesystem
    /// markers for [`LocalTransport`], GCS-native `ifGenerationMatch=0` CAS
    /// markers for the production backend) so the "publish each fold exactly
    /// once" invariant holds across pods.
    ///
    /// # Per-task telemetry (#328 Phase 1)
    ///
    /// `telemetry` carries the application-derived facts (peak RSS, pre-state
    /// provenance, phase timers, cold/warm flag) that a backend able to publish
    /// completion events folds into the [`ProverEvent`]. Threading ONE struct
    /// instead of six scalar params keeps the trait/impl churn minimal. The
    /// [`LocalTransport`] has no event bus and treats `telemetry` as a no-op
    /// (debug-logged); only the Pub/Sub path publishes it.
    fn commit_and_gate(
        &self,
        descriptor: &WorkDescriptor,
        bytes: &[u8],
        prove_time_ms: u64,
        total_time_ms: u64,
        telemetry: &TaskTelemetry,
    ) -> CommitOutcome;
}

/// A leased message. Holds the [`WorkDescriptor`] and the consumption verbs.
/// Dropping a lease without `ack`/`nack` is treated as a `nack` (the message
/// becomes visible again after the visibility timeout), matching Pub/Sub's
/// at-least-once redelivery on an un-acked, lease-expired message.
pub trait WorkLease {
    /// The descriptor carried by this lease.
    fn descriptor(&self) -> &WorkDescriptor;

    /// Extend the lease / ack deadline (heartbeat) while still working. A real
    /// backend calls `modifyAckDeadline`; the local backend pushes out the
    /// in-process visibility deadline.
    fn extend(&self);

    /// Acknowledge the message — call only **after** the result is durably
    /// committed. Removes the message from the queue.
    fn ack(self);

    /// Negative-acknowledge / abandon the message on failure, making it visible
    /// for redelivery (after the visibility timeout for the local backend).
    fn nack(self);
}

// ─────────────────────────────────────────────────────────────────────────
// LocalTransport — in-process queue + filesystem CAS commit (dev/test only)
// ─────────────────────────────────────────────────────────────────────────

/// One queued message and its in-flight state.
#[derive(Clone, Debug)]
struct QueuedMessage {
    descriptor: WorkDescriptor,
    /// When `Some`, the message is leased (invisible) until this instant.
    visible_at: Option<Instant>,
}

#[derive(Default)]
struct LocalQueue {
    messages: Vec<QueuedMessage>,
    /// Monotonic id for leases (debug/telemetry only).
    next_id: u64,
}

/// In-process work queue + filesystem proof store implementing [`WorkTransport`]
/// for development and tests. **Not** for the cross-node production path.
///
/// * The queue is an in-memory `Vec` guarded by a `Mutex`, with a visibility
///   timeout so an un-acked, lease-expired message is redelivered (modelling
///   Pub/Sub at-least-once).
/// * `commit_output` is an atomic create-if-absent via filesystem `O_EXCL`
///   (`create_new`), which is atomic on a single local filesystem — see the
///   contract note on [`WorkTransport::commit_output`]; the production backend
///   must use native object-store CAS instead.
/// * Readiness gating is filesystem-backed: committing a child output atomically
///   bumps the parent's completion count and, when the parent's real-child quota
///   is met, publishes that parent's fold descriptor **exactly once** (guarded
///   by an `O_EXCL` marker so only one committer publishes).
#[derive(Clone)]
pub struct LocalTransport {
    queue: Arc<Mutex<LocalQueue>>,
    /// Directory backing the proof store (committed outputs) and gating counters.
    store_dir: PathBuf,
    /// Visibility timeout for leased-but-un-acked messages.
    visibility: Duration,
    /// Whether to auto-publish readiness-gated parent folds on child commit.
    auto_gate: bool,
}

impl LocalTransport {
    /// Create a transport whose proof store + gating state live under
    /// `store_dir`. The directory is created if missing.
    pub fn new(store_dir: impl AsRef<Path>) -> Self {
        let store_dir = store_dir.as_ref().to_path_buf();
        fs::create_dir_all(&store_dir).expect("Failed to create LocalTransport store dir");
        Self {
            queue: Arc::new(Mutex::new(LocalQueue::default())),
            store_dir,
            visibility: Duration::from_secs(300),
            auto_gate: true,
        }
    }

    /// Override the visibility timeout (default 300s).
    pub fn with_visibility(mut self, visibility: Duration) -> Self {
        self.visibility = visibility;
        self
    }

    /// Disable automatic readiness-gating (parent-fold auto-publish on child
    /// commit). Useful for unit tests that want to drive gating manually.
    pub fn without_auto_gating(mut self) -> Self {
        self.auto_gate = false;
        self
    }

    /// Filesystem path for an output key.
    fn output_path(&self, key: &str) -> PathBuf {
        self.store_dir.join(key)
    }

    /// Path of the per-parent completion-count directory for the parent of a
    /// just-committed child. Each committed child drops a uniquely-named marker
    /// file in it; the directory entry count is the completion count, which is
    /// inherently idempotent (a re-commit of the same child writes the same
    /// marker name via `O_EXCL`, so it cannot double-count).
    fn gate_dir(&self, level: usize, node_idx: usize) -> PathBuf {
        self.store_dir
            .join(format!(".gate_L{level}_N{node_idx}"))
    }

    /// After a child output is committed, advance readiness for its parent and,
    /// if the parent is now ready, publish the parent fold descriptor exactly
    /// once. `committed` indicates whether THIS caller won the CAS (only the
    /// winner advances the count, so redeliveries don't inflate it).
    fn maybe_publish_parent(&self, child: &WorkDescriptor, committed: bool) {
        if !self.auto_gate || !committed {
            return;
        }
        // Determine the child's (level, idx) in the tree's level numbering.
        // Leaves live at level 0; a level-L node's children live at level L-1.
        let (child_level, child_idx) = match child.role {
            Role::Leaf => (0usize, child.chunk_idx),
            Role::TreeNode => (child.level, child.node_idx),
            Role::RootCoordinator => return, // root has no parent to publish
            // (#321 Phase 3) ReductionFold uses the interval-addressed,
            // opportunistic adjacent-pair gating introduced in Phase 4
            // (`maybe_publish_merge`), NOT this fixed-node hex gating. Until that
            // lands, a reduction commit publishes no parent here. Reduction
            // descriptors are not dispatched by any seeded run yet (the
            // `--fold-strategy` flag defaults to Hex and is not routed into
            // dispatch until Phase 4), so this arm is not reached at runtime.
            Role::ReductionFold => return,
        };

        let radix = child.radix;
        let leaf_count = child.leaf_count;
        let depth = tree_depth(leaf_count, radix);
        // A child at `child_level` feeds parents at `child_level + 1`. If that
        // exceeds the tree depth, the child IS the root — nothing to publish.
        let parent_level = child_level + 1;
        if parent_level > depth {
            return;
        }
        let parent_idx = child_idx / radix;
        let needed = real_children_for_node(leaf_count, radix, parent_level, parent_idx);

        // Record this child's completion (idempotent via O_EXCL marker name).
        let gate = self.gate_dir(parent_level, parent_idx);
        fs::create_dir_all(&gate).expect("Failed to create gate dir");
        let marker = gate.join(format!("child_{child_idx}"));
        let _ = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker);

        // Count distinct committed children.
        let have = fs::read_dir(&gate)
            .map(|rd| rd.filter_map(|e| e.ok()).count())
            .unwrap_or(0);

        if have >= needed {
            // Publish the parent fold exactly once, guarded by a published
            // marker so concurrent last-children don't double-publish.
            let pub_marker = self
                .store_dir
                .join(format!(".published_L{parent_level}_N{parent_idx}"));
            let won = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&pub_marker)
                .is_ok();
            if won {
                self.publish(WorkDescriptor::tree_node(
                    parent_level,
                    parent_idx,
                    radix,
                    leaf_count,
                    child.tx_per_proof,
                ));
            }
        }
    }
}

/// A lease handed out by [`LocalTransport`].
pub struct LocalLease {
    transport: LocalTransport,
    descriptor: WorkDescriptor,
    /// Stable identity of this message inside the queue (its descriptor is the
    /// key; descriptors for distinct work are distinct).
    acked: bool,
}

impl LocalLease {
    fn finish(&mut self, ack: bool) {
        let mut q = self.transport.queue.lock().expect("queue mutex poisoned");
        if let Some(pos) = q
            .messages
            .iter()
            .position(|m| m.descriptor == self.descriptor && m.visible_at.is_some())
        {
            if ack {
                q.messages.remove(pos);
            } else {
                // Make visible again immediately on explicit nack.
                q.messages[pos].visible_at = None;
            }
        }
        self.acked = true;
    }
}

impl WorkLease for LocalLease {
    fn descriptor(&self) -> &WorkDescriptor {
        &self.descriptor
    }

    fn extend(&self) {
        let mut q = self.transport.queue.lock().expect("queue mutex poisoned");
        if let Some(m) = q
            .messages
            .iter_mut()
            .find(|m| m.descriptor == self.descriptor && m.visible_at.is_some())
        {
            m.visible_at = Some(Instant::now() + self.transport.visibility);
        }
    }

    fn ack(mut self) {
        self.finish(true);
    }

    fn nack(mut self) {
        self.finish(false);
    }
}

impl Drop for LocalLease {
    fn drop(&mut self) {
        // Un-acked drop == nack (redeliver after visibility), matching Pub/Sub.
        if !self.acked {
            self.finish(false);
        }
    }
}

impl WorkTransport for LocalTransport {
    type Lease = LocalLease;

    fn pull_one(&self) -> Option<Self::Lease> {
        let mut q = self.queue.lock().expect("queue mutex poisoned");
        let now = Instant::now();
        let next_id = q.next_id;
        // Find the first visible message (not currently leased, or lease expired).
        let pos = q.messages.iter().position(|m| match m.visible_at {
            None => true,
            Some(deadline) => deadline <= now,
        })?;
        q.next_id = next_id.wrapping_add(1);
        q.messages[pos].visible_at = Some(now + self.visibility);
        let descriptor = q.messages[pos].descriptor.clone();
        Some(LocalLease {
            transport: self.clone(),
            descriptor,
            acked: false,
        })
    }

    fn publish(&self, descriptor: WorkDescriptor) {
        let mut q = self.queue.lock().expect("queue mutex poisoned");
        // De-dupe exact duplicates already pending (idempotent publish).
        if q.messages.iter().any(|m| m.descriptor == descriptor) {
            return;
        }
        q.messages.push(QueuedMessage {
            descriptor,
            visible_at: None,
        });
    }

    fn commit_output(&self, key: &str, bytes: &[u8]) -> CommitOutcome {
        let path = self.output_path(key);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("Failed to create output parent dir");
        }
        // Atomic CAS create-if-absent via O_EXCL. NOTE: atomic only on a single
        // local filesystem — production backends must use native object-store
        // CAS (GCS ifGenerationMatch=0); see the trait contract.
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                f.write_all(bytes).expect("Failed to write committed output");
                f.sync_all().ok();
                CommitOutcome::Committed
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                CommitOutcome::AlreadyExists
            }
            Err(e) => panic!("Failed to commit output {key}: {e:?}"),
        }
    }

    fn output_exists(&self, key: &str) -> bool {
        self.output_path(key).exists()
    }

    fn read_output(&self, key: &str) -> Option<Vec<u8>> {
        fs::read(self.output_path(key)).ok()
    }

    /// Commit a child's output **and** advance readiness gating in one call:
    /// commits `bytes` under `descriptor.output_key()`, then (if this caller won
    /// the CAS) publishes the parent fold once the parent's child quota is met.
    /// This is the primitive the dispatch loop uses so that completing the last
    /// child of a node automatically enqueues that node's fold. Implemented as
    /// the [`WorkTransport`] trait method so the generic dispatch loop drives it.
    fn commit_and_gate(
        &self,
        descriptor: &WorkDescriptor,
        bytes: &[u8],
        _prove_time_ms: u64,
        _total_time_ms: u64,
        telemetry: &TaskTelemetry,
    ) -> CommitOutcome {
        // Local dev/test transport has no event bus, so per-task telemetry
        // (#328) is not published — only observed here at debug for parity.
        log::debug!(
            "[LocalTransport] commit_and_gate {} telemetry: peak_rss_bytes={} \
             prestate_source={} is_first_task_on_pod={}",
            descriptor.output_key(),
            telemetry.peak_rss_bytes,
            telemetry.prestate_source.as_str(),
            telemetry.is_first_task_on_pod,
        );
        let outcome = self.commit_output(&descriptor.output_key(), bytes);
        self.maybe_publish_parent(descriptor, outcome == CommitOutcome::Committed);
        outcome
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Tree geometry (shared with prover_node.rs; duplicated here so the transport
// crate is self-contained and unit-testable without the binary).
// ─────────────────────────────────────────────────────────────────────────

/// Depth of the radix-ary reduction tree over `n` leaves: `ceil(log_radix(n))`.
/// Computed iteratively to avoid float rounding near exact powers.
pub fn tree_depth(n: usize, radix: usize) -> usize {
    assert!(radix >= 2, "radix must be >= 2");
    if n <= 1 {
        return 0;
    }
    let mut depth = 0usize;
    let mut span = 1usize;
    while span < n {
        span = span.saturating_mul(radix);
        depth += 1;
    }
    depth
}

/// Number of nodes at `level` (>= 1): `ceil(n / radix^level)`, min 1.
pub fn nodes_at_level(n: usize, radix: usize, level: usize) -> usize {
    assert!(level >= 1, "tree levels are 1-indexed");
    assert!(radix >= 2, "radix must be >= 2");
    let mut divisor = 1usize;
    for _ in 0..level {
        divisor = divisor.saturating_mul(radix);
    }
    n.div_ceil(divisor).max(1)
}

/// Number of real (non-padding) children node `node_idx` at `level` has.
pub fn real_children_for_node(n: usize, radix: usize, level: usize, node_idx: usize) -> usize {
    let children_population = if level == 1 {
        n
    } else {
        nodes_at_level(n, radix, level - 1)
    };
    let first = node_idx * radix;
    if first >= children_population {
        return 0;
    }
    (children_population - first).min(radix)
}

/// Fail-fast validation of a seed plan, surfaced on the seeder/worker path so an
/// invalid plan is rejected with a clear message BEFORE any descriptor is
/// published (issue #310). `available_chunks` is `ceil(block_tx_count / C)`,
/// the number of real tx chunks the worker can actually prove; a `leaf_count`
/// that exceeds it would address a non-existent chunk and panic IN THE POD.
///
/// This is the transport-crate guard that complements the binary's richer
/// `WorkloadPlan::derive`: any code path that seeds (the binary, a future
/// orchestrator, or a test) can call this so the worker path "can't silently
/// seed an invalid plan".
pub fn validate_seed_plan(
    radix: usize,
    leaf_count: usize,
    tx_per_proof: usize,
    available_chunks: usize,
) -> Result<(), String> {
    if radix < 2 {
        return Err(format!("radix must be >= 2 (got {radix})"));
    }
    if leaf_count == 0 {
        return Err("leaf_count must be >= 1 (got 0); nothing to prove".to_string());
    }
    if tx_per_proof == 0 {
        return Err("tx_per_proof must be >= 1 (got 0); each leaf must carry >= 1 tx".to_string());
    }
    if leaf_count > available_chunks {
        return Err(format!(
            "leaf_count {leaf_count} exceeds available chunks {available_chunks}; \
             the worker would address a non-existent chunk and panic in-pod"
        ));
    }
    Ok(())
}

/// Seed the descriptors needed to prove an N-leaf tree from scratch: one leaf
/// descriptor per leaf. Readiness gating then publishes the fold tasks level by
/// level as children complete. Returned in deterministic order for tests.
pub fn seed_leaf_descriptors(
    radix: usize,
    leaf_count: usize,
    tx_per_proof: usize,
) -> Vec<WorkDescriptor> {
    (0..leaf_count)
        .map(|i| WorkDescriptor::leaf(i, radix, leaf_count, tx_per_proof))
        .collect()
}

// Re-export the backend-agnostic CAS + gating primitives so callers and the
// production backend can `use bench::transport::{ObjectStore, GatingEngine, ...}`.
pub use gating::{CasStore, GatingEngine, GatingOutcome, InMemoryCasStore, Publisher};

// ─────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A neutral telemetry value for gating tests that only exercise the CAS +
    /// publish semantics (the telemetry param is a no-op on `LocalTransport`).
    fn test_telem() -> TaskTelemetry {
        TaskTelemetry::new(0, crate::telemetry::PrestateSource::NotApplicable, false)
    }

    fn tmp_store(tag: &str) -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "bench-transport-test-{tag}-{}-{nanos}-{seq}",
            std::process::id(),
        ));
        p
    }

    #[test]
    fn descriptor_output_keys_match_fs_transport_naming() {
        let leaf = WorkDescriptor::leaf(3, 2, 4, 1);
        assert_eq!(leaf.output_key(), "leaf_3.proof");
        let node = WorkDescriptor::tree_node(2, 1, 2, 4, 1);
        assert_eq!(node.output_key(), "tree_L2_N1.proof");
        let root = WorkDescriptor::root(2, 4, 1);
        assert_eq!(root.output_key(), "root_verified.marker");

        // (#321 Phase 3) reduction-fold output is interval-addressed.
        let red = WorkDescriptor::reduction_fold(0, 3, 2, 2, 4, 1);
        assert_eq!(red.output_key(), "reduction_0_3.proof");
        assert_eq!(red.lo, 0);
        assert_eq!(red.hi, 3);
        assert_eq!(red.level, 2);
        assert_eq!(red.role, Role::ReductionFold);
    }

    #[test]
    fn role_tags_are_stable() {
        assert_eq!(Role::Leaf.as_str(), "leaf");
        assert_eq!(Role::TreeNode.as_str(), "tree-node");
        assert_eq!(Role::RootCoordinator.as_str(), "root-coordinator");
        assert_eq!(Role::ReductionFold.as_str(), "reduction-fold");
    }

    /// (#321 Phase 3) A descriptor serialized BEFORE the interval fields existed
    /// (no `lo`/`hi`) must still deserialize — `#[serde(default)]` supplies 0/0.
    /// This guards wire back-compat for in-flight pre-Phase-3 descriptors.
    #[test]
    fn pre_phase3_descriptor_without_interval_fields_still_deserializes() {
        let legacy = r#"{
            "role":"tree-node","radix":2,"leaf_count":4,"tx_per_proof":1,
            "chunk_idx":0,"level":1,"node_idx":0
        }"#;
        let d: WorkDescriptor = serde_json::from_str(legacy).expect("legacy descriptor must deserialize");
        assert_eq!(d.lo, 0, "missing lo must default to 0");
        assert_eq!(d.hi, 0, "missing hi must default to 0");
        assert_eq!(d.role, Role::TreeNode);
        assert_eq!(d.level, 1);
    }

    /// (#321 Phase 3) A reduction-fold descriptor round-trips through JSON with
    /// its interval intact.
    #[test]
    fn reduction_fold_descriptor_round_trips_through_json() {
        let d = WorkDescriptor::reduction_fold(4, 7, 2, 2, 8, 1);
        let s = serde_json::to_string(&d).unwrap();
        let back: WorkDescriptor = serde_json::from_str(&s).unwrap();
        assert_eq!(d, back);
        assert_eq!(back.output_key(), "reduction_4_7.proof");
    }

    #[test]
    fn descriptor_round_trips_through_json() {
        let d = WorkDescriptor::tree_node(2, 1, 16, 300, 4);
        let s = serde_json::to_string(&d).unwrap();
        let back: WorkDescriptor = serde_json::from_str(&s).unwrap();
        assert_eq!(d, back);
        // role serialises kebab-case
        assert!(s.contains("\"tree-node\""), "got: {s}");
    }

    /// (#328 Phase 1) A `ProverEvent` carrying the new telemetry fields round-
    /// trips through JSON with every field intact.
    #[test]
    fn prover_event_with_telemetry_round_trips_through_json() {
        let event = ProverEvent {
            descriptor: WorkDescriptor::leaf(3, 2, 8, 300),
            status: "success".to_string(),
            prove_time_ms: 1234,
            gcs_time_ms: 56,
            total_time_ms: 1400,
            peak_rss_bytes: 4_294_967_296,
            prestate_source: "corpus".to_string(),
            pull_ms: 7,
            pre_exec_ms: 0,
            prove_ms: 1234,
            gcs_write_ms: 56,
            queue_wait_ms: 0,
            is_first_task_on_pod: true,
            chunk_size: 300,
            leaf_count: 8,
        };
        let s = serde_json::to_string(&event).unwrap();
        let back: ProverEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back.status, "success");
        assert_eq!(back.prove_time_ms, 1234);
        assert_eq!(back.gcs_time_ms, 56);
        assert_eq!(back.total_time_ms, 1400);
        assert_eq!(back.peak_rss_bytes, 4_294_967_296);
        assert_eq!(back.prestate_source, "corpus");
        assert_eq!(back.pull_ms, 7);
        assert_eq!(back.pre_exec_ms, 0);
        assert_eq!(back.prove_ms, 1234);
        assert_eq!(back.gcs_write_ms, 56);
        assert_eq!(back.queue_wait_ms, 0);
        assert!(back.is_first_task_on_pod);
        assert_eq!(back.chunk_size, 300);
        assert_eq!(back.leaf_count, 8);
        assert_eq!(back.descriptor, WorkDescriptor::leaf(3, 2, 8, 300));
    }

    /// (#328 Phase 1) A pre-#328 `ProverEvent` JSON carrying ONLY the original
    /// five fields must still deserialize — `#[serde(default)]` supplies the new
    /// telemetry fields (`0` / `"n/a"` / `false`). This guards wire back-compat
    /// for in-flight pre-#328 events, mirroring the Phase-3 descriptor test.
    #[test]
    fn pre_phase328_event_without_telemetry_fields_still_deserializes() {
        let legacy = r#"{
            "descriptor":{
                "role":"leaf","radix":2,"leaf_count":8,"tx_per_proof":300,
                "chunk_idx":3,"level":0,"node_idx":0,"lo":0,"hi":0
            },
            "status":"success",
            "prove_time_ms":1234,
            "gcs_time_ms":56,
            "total_time_ms":1400
        }"#;
        let e: ProverEvent =
            serde_json::from_str(legacy).expect("legacy event must deserialize");
        // Original fields intact.
        assert_eq!(e.status, "success");
        assert_eq!(e.prove_time_ms, 1234);
        assert_eq!(e.gcs_time_ms, 56);
        assert_eq!(e.total_time_ms, 1400);
        // New telemetry fields default honestly.
        assert_eq!(e.peak_rss_bytes, 0, "missing peak_rss_bytes defaults to 0");
        assert_eq!(
            e.prestate_source, "n/a",
            "missing prestate_source defaults to the n/a sentinel"
        );
        assert_eq!(e.pull_ms, 0);
        assert_eq!(e.pre_exec_ms, 0);
        assert_eq!(e.prove_ms, 0);
        assert_eq!(e.gcs_write_ms, 0);
        assert_eq!(e.queue_wait_ms, 0);
        assert!(!e.is_first_task_on_pod, "missing flag defaults to false");
        assert_eq!(e.chunk_size, 0);
        assert_eq!(e.leaf_count, 0);
    }

    #[test]
    fn pull_publish_ack_basic() {
        let t = LocalTransport::new(tmp_store("basic")).without_auto_gating();
        assert!(t.pull_one().is_none(), "empty queue yields nothing");
        t.publish(WorkDescriptor::leaf(0, 2, 4, 1));
        let lease = t.pull_one().expect("one message available");
        assert_eq!(lease.descriptor().chunk_idx, 0);
        // While leased, no other pull sees it (flow-control = 1 outstanding).
        assert!(t.pull_one().is_none(), "leased message is invisible");
        lease.ack();
        assert!(t.pull_one().is_none(), "acked message is gone");
    }

    #[test]
    fn nack_redelivers_immediately() {
        let t = LocalTransport::new(tmp_store("nack")).without_auto_gating();
        t.publish(WorkDescriptor::leaf(0, 2, 4, 1));
        let lease = t.pull_one().unwrap();
        lease.nack();
        // After nack the message is visible again.
        let lease2 = t.pull_one().expect("nacked message redelivered");
        assert_eq!(lease2.descriptor().chunk_idx, 0);
        lease2.ack();
    }

    #[test]
    fn dropped_lease_redelivers() {
        let t = LocalTransport::new(tmp_store("drop")).without_auto_gating();
        t.publish(WorkDescriptor::leaf(7, 2, 8, 1));
        {
            let _lease = t.pull_one().unwrap();
            // dropped without ack/nack
        }
        let lease2 = t.pull_one().expect("dropped lease redelivers");
        assert_eq!(lease2.descriptor().chunk_idx, 7);
        lease2.ack();
    }

    #[test]
    fn visibility_timeout_redelivers() {
        let t = LocalTransport::new(tmp_store("vis"))
            .without_auto_gating()
            .with_visibility(Duration::from_millis(50));
        t.publish(WorkDescriptor::leaf(0, 2, 2, 1));
        let lease = t.pull_one().unwrap();
        // Leak the lease so Drop doesn't nack it; rely on the timeout instead.
        std::mem::forget(lease);
        assert!(t.pull_one().is_none(), "still leased before timeout");
        thread::sleep(Duration::from_millis(80));
        let lease2 = t.pull_one().expect("redelivered after visibility timeout");
        std::mem::forget(lease2);
    }

    #[test]
    fn extend_pushes_out_visibility() {
        let t = LocalTransport::new(tmp_store("extend"))
            .without_auto_gating()
            .with_visibility(Duration::from_millis(60));
        t.publish(WorkDescriptor::leaf(0, 2, 2, 1));
        let lease = t.pull_one().unwrap();
        thread::sleep(Duration::from_millis(40));
        lease.extend(); // reset the 60ms clock
        thread::sleep(Duration::from_millis(40)); // 80ms since pull, but extended
        assert!(
            t.pull_one().is_none(),
            "extend should keep the message leased"
        );
        std::mem::forget(lease);
    }

    #[test]
    fn commit_output_is_idempotent_single_thread() {
        let t = LocalTransport::new(tmp_store("idem"));
        assert_eq!(t.commit_output("k", b"first"), CommitOutcome::Committed);
        assert_eq!(
            t.commit_output("k", b"second"),
            CommitOutcome::AlreadyExists
        );
        // First writer's bytes win.
        assert_eq!(t.read_output("k").unwrap(), b"first");
        assert!(t.output_exists("k"));
    }

    /// The acceptance-criteria concurrency test: N threads commit the SAME key;
    /// exactly one observes `Committed`, the rest `AlreadyExists`, and the
    /// stored bytes are exactly the winner's (never interleaved).
    #[test]
    fn commit_output_exactly_one_winner_under_concurrency() {
        let t = LocalTransport::new(tmp_store("cas-race"));
        const N: usize = 32;
        let committed = Arc::new(AtomicUsize::new(0));
        let already = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for i in 0..N {
            let t = t.clone();
            let committed = committed.clone();
            let already = already.clone();
            handles.push(thread::spawn(move || {
                // Each thread writes its own distinct payload; only the winner's
                // bytes may survive.
                let payload = format!("winner-{i}");
                match t.commit_output("shared.key", payload.as_bytes()) {
                    CommitOutcome::Committed => {
                        committed.fetch_add(1, Ordering::SeqCst);
                    }
                    CommitOutcome::AlreadyExists => {
                        already.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            committed.load(Ordering::SeqCst),
            1,
            "exactly one thread must win the CAS create"
        );
        assert_eq!(
            already.load(Ordering::SeqCst),
            N - 1,
            "every other thread must see AlreadyExists"
        );
        // Stored bytes must be exactly one thread's payload (not interleaved).
        let stored = String::from_utf8(t.read_output("shared.key").unwrap()).unwrap();
        assert!(
            stored.starts_with("winner-"),
            "stored bytes must be a single winner's payload, got {stored:?}"
        );
    }

    #[test]
    fn readiness_gating_publishes_parent_when_children_complete() {
        // radix=2, N=4 => level 1 node 0 folds leaves {0,1}.
        let t = LocalTransport::new(tmp_store("gate"));
        let l0 = WorkDescriptor::leaf(0, 2, 4, 1);
        let l1 = WorkDescriptor::leaf(1, 2, 4, 1);

        // Commit first child: parent not yet ready, nothing published.
        assert_eq!(t.commit_and_gate(&l0, b"leaf0", 0, 0, &test_telem()), CommitOutcome::Committed);
        assert!(
            t.pull_one().is_none(),
            "parent fold must not publish until all children done"
        );

        // Commit second child: parent ready, fold descriptor published.
        assert_eq!(t.commit_and_gate(&l1, b"leaf1", 0, 0, &test_telem()), CommitOutcome::Committed);
        let lease = t.pull_one().expect("parent fold should be published");
        let d = lease.descriptor();
        assert_eq!(d.role, Role::TreeNode);
        assert_eq!(d.level, 1);
        assert_eq!(d.node_idx, 0);
        lease.ack();
    }

    #[test]
    fn readiness_gating_publishes_root_parent_for_level1_nodes() {
        // radix=2, N=4 => depth 2; level-1 nodes {0,1} fold into level-2 root.
        let t = LocalTransport::new(tmp_store("gate-root"));
        let n0 = WorkDescriptor::tree_node(1, 0, 2, 4, 1);
        let n1 = WorkDescriptor::tree_node(1, 1, 2, 4, 1);

        assert_eq!(t.commit_and_gate(&n0, b"node10", 0, 0, &test_telem()), CommitOutcome::Committed);
        assert!(t.pull_one().is_none(), "root not ready after one level-1 node");

        assert_eq!(t.commit_and_gate(&n1, b"node11", 0, 0, &test_telem()), CommitOutcome::Committed);
        let lease = t.pull_one().expect("root fold should publish");
        let d = lease.descriptor();
        assert_eq!(d.role, Role::TreeNode);
        assert_eq!(d.level, 2);
        assert_eq!(d.node_idx, 0);
        lease.ack();
    }

    #[test]
    fn gating_re_commit_does_not_double_count() {
        // A redelivered child (AlreadyExists on commit) must not advance gating.
        let t = LocalTransport::new(tmp_store("gate-redeliver"));
        let l0 = WorkDescriptor::leaf(0, 2, 4, 1);
        assert_eq!(t.commit_and_gate(&l0, b"leaf0", 0, 0, &test_telem()), CommitOutcome::Committed);
        // Re-commit the SAME child (simulating redelivery): AlreadyExists, and
        // it must NOT make the parent (which still needs leaf 1) ready.
        assert_eq!(
            t.commit_and_gate(&l0, b"leaf0-dup", 0, 0, &test_telem()),
            CommitOutcome::AlreadyExists
        );
        assert!(
            t.pull_one().is_none(),
            "redelivered child must not falsely satisfy the parent quota"
        );
    }

    // ── geometry helpers ──

    #[test]
    fn geometry_matches_prover_node() {
        assert_eq!(tree_depth(4, 2), 2);
        assert_eq!(tree_depth(8, 2), 3);
        assert_eq!(nodes_at_level(4, 2, 1), 2);
        assert_eq!(nodes_at_level(4, 2, 2), 1);
        assert_eq!(real_children_for_node(4, 2, 1, 0), 2);
        assert_eq!(real_children_for_node(5, 2, 1, 2), 1);
    }

    #[test]
    fn seed_descriptors_count() {
        let seeds = seed_leaf_descriptors(2, 4, 1);
        assert_eq!(seeds.len(), 4);
        assert!(seeds.iter().all(|d| d.role == Role::Leaf));
        assert_eq!(seeds[3].chunk_idx, 3);
    }

    #[test]
    fn validate_seed_plan_rejects_leaf_count_over_available_chunks() {
        // 500-tx block, C=5 ⇒ available chunks = 100. leaf_count <= 100 OK.
        assert!(validate_seed_plan(16, 100, 5, 100).is_ok());
        // leaf_count 101 > 100 would address a non-existent chunk ⇒ rejected.
        let err = validate_seed_plan(16, 101, 5, 100).unwrap_err();
        assert!(err.contains("exceeds available chunks"), "got: {err}");
    }

    #[test]
    fn validate_seed_plan_rejects_degenerate_inputs() {
        assert!(validate_seed_plan(1, 4, 1, 4).unwrap_err().contains("radix"));
        assert!(validate_seed_plan(2, 0, 1, 4).unwrap_err().contains("leaf_count"));
        assert!(validate_seed_plan(2, 4, 0, 4).unwrap_err().contains("tx_per_proof"));
    }
}
