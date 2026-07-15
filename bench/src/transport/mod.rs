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

/// (#321 Phase 5) The fold strategy a descriptor belongs to, carried on the WIRE
/// so the coordinator can ROUTE a completion event to the correct gate: the hex
/// fixed-node gate ([`GatingEngine::on_child_committed`]) for [`FoldStrategy::Hex`],
/// or the order-free interval gate ([`GatingEngine::on_interval_committed`]) for
/// [`FoldStrategy::Reduction`].
///
/// This is DISTINCT from the CLI `FoldStrategy` value-enum in `prover_node.rs`
/// (a `clap::ValueEnum` for command-line parsing): this one is the serialized
/// contract between worker and coordinator. `#[serde(rename_all = "kebab-case")]`
/// matches [`Role`]'s style. [`FoldStrategy::Hex`] is the [`Default`] so a
/// descriptor serialized BEFORE this field existed deserializes to the existing
/// hex path — wire back-compat, exactly like the Phase-3 `lo`/`hi` addition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum FoldStrategy {
    /// The existing radix-16 hexadecimal fixed-node fold. The default so
    /// pre-Phase-5 descriptors (which lack the field) route to the hex gate
    /// unchanged.
    #[default]
    Hex,
    /// The order-free same-height binary reduction fold (issue #321), gated by
    /// interval-addressed adjacent-pair merging.
    Reduction,
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
    /// (#321 Phase 5) Which fold strategy this descriptor belongs to, so the
    /// coordinator can ROUTE a completion event to the correct gate. Defaults to
    /// [`FoldStrategy::Hex`] via `#[serde(default)]` so descriptors serialized
    /// before Phase 5 (which lack this field) route to the existing hex path —
    /// wire back-compat, mirroring the Phase-3 `lo`/`hi` addition.
    #[serde(default)]
    pub fold_strategy: FoldStrategy,
    /// (#321 Phase 5) `true` when THIS descriptor was published by a crash-
    /// recovery re-drive ([`GatingEngine::redrive_stale_merges`]) rather than the
    /// normal gate — so the completion event can flag `redriven_after_lease_expiry`
    /// and the coordinator can count stale-lease re-drives. `#[serde(default)]`
    /// (= `false`) for wire back-compat and because normal-gate publishes never
    /// set it.
    #[serde(default)]
    pub redriven: bool,
    /// (#321 Phase 6) Epoch-milliseconds when THIS descriptor was PUBLISHED /
    /// SEEDED for dispatch — the timestamp a worker subtracts on pull to compute
    /// the HONEST `queue_wait_ms` (#328 Phase 1 emitted it as `0` because this
    /// plumbing did not exist). Stamped at the actual PUBLISH BOUNDARY (the
    /// seeder and the concrete production [`Publisher`] impls), NOT inside the
    /// pure [`GatingEngine`], so the engine stays unit-testable and existing
    /// descriptor-equality assertions hold. `#[serde(default)]` (= `0`) for wire
    /// back-compat AND as the honest "not stamped" sentinel: a `0` here means the
    /// dispatch time was never recorded, so `queue_wait_ms` stays `0` rather than
    /// being fabricated from a bogus baseline. Anti-fabrication: NEVER invent a
    /// plausible-looking timestamp; `0` is the truthful "unknown".
    #[serde(default)]
    pub dispatch_ts_ms: u64,
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
            fold_strategy: FoldStrategy::Hex,
            redriven: false,
            // Pure constructor: unstamped (0). The seeder/publisher boundary
            // stamps the real dispatch time; 0 is the honest "not stamped".
            dispatch_ts_ms: 0,
        }
    }

    /// (#321 Phase 5) A REDUCTION leaf descriptor for chunk `chunk_idx`: a
    /// [`Role::Leaf`] tagged [`FoldStrategy::Reduction`] with its interval seeded
    /// to `[chunk_idx, chunk_idx]` at level 0. Seeding a reduction run uses this
    /// (instead of [`WorkDescriptor::leaf`]) so the coordinator routes the leaf's
    /// completion to the interval gate ([`GatingEngine::on_interval_committed`])
    /// with the correct single-leaf interval `[i, i]` at level 0 — the base of
    /// the padded perfect binary reduction tree.
    pub fn reduction_leaf(
        chunk_idx: usize,
        radix: usize,
        leaf_count: usize,
        tx_per_proof: usize,
    ) -> Self {
        Self {
            role: Role::Leaf,
            radix,
            leaf_count,
            tx_per_proof,
            chunk_idx,
            // A reduction leaf's interval is the single leaf [chunk_idx, chunk_idx]
            // at reduction level 0 (the tree base). lo == hi == chunk_idx.
            level: 0,
            node_idx: 0,
            lo: chunk_idx,
            hi: chunk_idx,
            fold_strategy: FoldStrategy::Reduction,
            redriven: false,
            dispatch_ts_ms: 0,
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
            fold_strategy: FoldStrategy::Hex,
            redriven: false,
            dispatch_ts_ms: 0,
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
            fold_strategy: FoldStrategy::Reduction,
            redriven: false,
            dispatch_ts_ms: 0,
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
            fold_strategy: FoldStrategy::Hex,
            redriven: false,
            dispatch_ts_ms: 0,
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

    /// (#321 Phase 6) Stamp this descriptor's [`dispatch_ts_ms`](Self::dispatch_ts_ms)
    /// with the CURRENT wall-clock epoch-millis and return `self`, so the seeder
    /// and the concrete [`Publisher`] impls can stamp AT THE PUBLISH BOUNDARY in
    /// one call (`WorkDescriptor::leaf(..).stamped_now()`). Deliberately NOT
    /// called from inside the pure [`GatingEngine`]: the engine emits unstamped
    /// (`dispatch_ts_ms = 0`) descriptors so its unit tests keep asserting exact
    /// descriptor fields, and stamping happens only where the descriptor is
    /// actually handed to the transport for a worker to pull.
    #[must_use]
    pub fn stamped_now(mut self) -> Self {
        self.dispatch_ts_ms = now_epoch_ms();
        self
    }

    /// (#321 Phase 6) Compute the HONEST queue-wait for this descriptor given the
    /// wall-clock `pull_ts_ms` at which a worker pulled it: `pull - dispatch`,
    /// but ONLY when [`dispatch_ts_ms`](Self::dispatch_ts_ms) was actually stamped
    /// (`> 0`). If the descriptor is UNSTAMPED (`dispatch_ts_ms == 0`, the honest
    /// sentinel) this returns `0` — the queue-wait was not measured, so we report
    /// `0` rather than fabricating a value from a bogus zero baseline.
    /// `saturating_sub` guards a clock skew that would put `pull` before
    /// `dispatch` (returns `0`, never a wrapped huge number).
    pub fn queue_wait_ms_from_pull(&self, pull_ts_ms: u64) -> u64 {
        if self.dispatch_ts_ms == 0 {
            // Honest sentinel: dispatch time was never recorded ⇒ not measurable.
            0
        } else {
            pull_ts_ms.saturating_sub(self.dispatch_ts_ms)
        }
    }
}

/// (#321 Phase 6) Current wall-clock time in milliseconds since the UNIX epoch.
/// The single source of the dispatch/pull timestamps that make `queue_wait_ms`
/// honest. Mirrors the gating engine's `now_lease_stamp_ms` style; degrades to
/// `0` if the system clock is before the epoch (never panics). Anti-fabrication:
/// callers store the REAL value or the honest `0` sentinel — never a made-up one.
pub fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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

    // ── #321 Phase 5: reduction / recovery telemetry (all serde(default)) ──────
    /// The kind of fold this task performed, so REAL folds can be sized
    /// separately from nearly-free PADDING no-op folds:
    ///   * `"real"`        — a real same-height fold of two real children.
    ///   * `"padding-noop"`— the `right_is_real = false` right-padding passthrough
    ///                       (nearly free; `BinaryTreeChainCircuit::prove_padding`).
    ///   * `"n/a"`         — leaf / hex / non-reduction task (no fold-kind concept).
    /// Defaults to `"n/a"` for pre-Phase-5 payloads via
    /// [`fold_kind_default`]. Anti-fabrication: never a made-up label.
    #[serde(default = "fold_kind_default")]
    pub fold_kind: String,
    /// For a reduction event, the merged interval span `hi - lo + 1`; `0` for
    /// non-reduction events (honest zero — a hex/leaf task has no interval span).
    #[serde(default)]
    pub merge_interval_span: usize,
    /// `true` if THIS task was published by a crash-recovery re-drive
    /// ([`GatingEngine::redrive_stale_merges`], Part C) rather than the normal
    /// gate. Surfaced from `descriptor.redriven`. Defaults to `false`.
    #[serde(default)]
    pub redriven_after_lease_expiry: bool,

    // ── #321 Phase 6: dispatch-scheduling telemetry (all serde(default)) ───────
    /// Epoch-milliseconds at which the worker PULLED this task off the queue.
    /// Paired across all events of a run this lets a later extractor compute the
    /// LEAF WAVE WIDTH = `max(pull_ts_ms) − min(pull_ts_ms)` (how spread-out the
    /// leaf pulls were — the observable a straggler-aware seed order is meant to
    /// tighten). `0` when the dispatch loop did not record it (honest sentinel —
    /// never fabricated). Defaults to `0` for pre-Phase-6 payloads.
    #[serde(default)]
    pub pull_ts_ms: u64,
    /// The seed-ordering strategy this run used, echoed so a run SELF-DESCRIBES
    /// which schedule produced it (`"sequential"` = the default `0..N` order,
    /// `"critical-path-first"` = the straggler-aware front-loading). Surfaced
    /// from [`SeedOrder::as_str`]. Defaults to `"sequential"` for pre-Phase-6
    /// payloads via [`scheduling_class_default`] — the historical behaviour.
    #[serde(default = "scheduling_class_default")]
    pub scheduling_class: String,
}

/// serde default for [`ProverEvent::scheduling_class`]: `"sequential"` so a
/// pre-Phase-6 payload (which omits the field) deserializes to the historical
/// default seed order rather than an empty string. Matches [`SeedOrder::Sequential`].
fn scheduling_class_default() -> String {
    "sequential".to_string()
}

/// serde default for [`ProverEvent::fold_kind`]: `"n/a"` so a pre-Phase-5
/// payload (which omits the field) deserializes to the leaf/hex/non-reduction
/// sentinel rather than an empty string.
fn fold_kind_default() -> String {
    "n/a".to_string()
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
        // #321 Phase 6: stamp the dispatch time AT THE PUBLISH BOUNDARY. A leaf
        // seeded via `seed_leaf_descriptors` is already stamped; a fold published
        // by `maybe_publish_parent` arrives unstamped (0) from the pure gating
        // path, so stamp it here — the true "published for dispatch" instant.
        let descriptor = descriptor.stamped_now();
        let mut q = self.queue.lock().expect("queue mutex poisoned");
        // De-dupe duplicates already pending (idempotent publish). Identity is the
        // OUTPUT KEY (the work unit), NOT full-struct equality: the Phase-6
        // `dispatch_ts_ms` stamp differs between a re-publish and the original, so
        // comparing full structs would wrongly treat a redelivery as new work.
        let key = descriptor.output_key();
        if q.messages.iter().any(|m| m.descriptor.output_key() == key) {
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

/// (#321 Phase 6) The order in which leaf descriptors are SEEDED onto the
/// transport — a pluggable, deterministic scheduling mechanism.
///
/// This is a SCHEDULING MECHANISM + telemetry, not a proven speedup: it controls
/// which leaves a worker pool pulls FIRST. `Sequential` preserves the historical
/// `0..N` behaviour (the default; existing callers are unchanged). `CriticalPathFirst`
/// front-loads the leaves that feed the LAST-to-merge top-level pair, so — IF a
/// straggler stalls a leaf — it is more likely to be one whose merge does not sit
/// on the final critical path. Whether that tightens the straggler tail is an
/// EMPIRICAL question answered by a real multi-pod run (out of scope here); this
/// phase only provides the deterministic order + the pull/dispatch timestamps a
/// benchmark needs to MEASURE the effect later.
///
/// Anti-fabrication: selecting `CriticalPathFirst` asserts NOTHING about
/// wall-clock improvement; it only changes the deterministic seed order and the
/// `scheduling_class` a run self-reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SeedOrder {
    /// Seed leaves in plain `0, 1, .., N-1` order (the historical default).
    #[default]
    Sequential,
    /// Seed leaves so the ones on the FINAL merge's critical path go FIRST.
    /// See [`seed_leaf_descriptors_scheduled`] for the exact deterministic order.
    CriticalPathFirst,
}

impl SeedOrder {
    /// The stable wire string echoed into
    /// [`ProverEvent::scheduling_class`](crate::transport::ProverEvent::scheduling_class).
    pub fn as_str(&self) -> &'static str {
        match self {
            SeedOrder::Sequential => "sequential",
            SeedOrder::CriticalPathFirst => "critical-path-first",
        }
    }
}

/// Seed the descriptors needed to prove an N-leaf tree from scratch: one leaf
/// descriptor per leaf, in plain `0..N` order. Readiness gating then publishes
/// the fold tasks level by level as children complete. Returned in deterministic
/// order for tests.
///
/// (#321 Phase 6) Each seeded leaf is STAMPED with `dispatch_ts_ms = now` at seed
/// time — this is the publish-boundary stamp a worker subtracts on pull to
/// compute the honest `queue_wait_ms`. This is the default `Sequential` order;
/// [`seed_leaf_descriptors_scheduled`] adds the opt-in straggler-aware order.
pub fn seed_leaf_descriptors(
    radix: usize,
    leaf_count: usize,
    tx_per_proof: usize,
) -> Vec<WorkDescriptor> {
    seed_leaf_descriptors_scheduled(radix, leaf_count, tx_per_proof, SeedOrder::Sequential)
}

/// (#321 Phase 6) Seed the N leaf descriptors in the requested [`SeedOrder`],
/// each STAMPED with `dispatch_ts_ms = now` at seed time (the publish-boundary
/// stamp for honest `queue_wait_ms`). Every returned `Vec` is a PERMUTATION of
/// the leaf indices `0..N` — each leaf appears EXACTLY ONCE — so no leaf is ever
/// dropped or duplicated regardless of order; only the *sequence* differs.
///
/// # `CriticalPathFirst` heuristic (deterministic, simple, documented)
///
/// The reduction tree is a padded PERFECT binary tree over `P = next_power_of_two(N)`
/// leaves; the ROOT is the merge of the two top-level halves `[0, P/2)` and
/// `[P/2, P)`, and each half is itself the merge of its two halves, recursively.
/// The LAST merge to run is the root, and it cannot fire until BOTH top-level
/// halves are complete — so a straggler anywhere delays the root. To front-load
/// the critical path we emit leaves in **bit-reversal-permutation order**: index
/// `i` maps to `bitrev(i, log2 P)`. Bit reversal interleaves the two halves
/// (`0, P/2, P/4, 3P/4, ...`), so the seed sequence alternates between the two
/// top-level subtrees from the very first leaves — every top-level pair, and
/// recursively every sub-pair, starts receiving leaves as early as possible
/// rather than finishing one whole half before the other begins. That makes the
/// last-to-complete merge least likely to sit idle waiting on a lone straggler
/// in a half that was seeded last.
///
/// Why bit reversal specifically:
/// * DETERMINISTIC and pure (a fixed function of `i` and `P`), so the order is
///   reproducible and unit-testable — no randomness, no clock dependence.
/// * A well-known PERMUTATION of `0..P` (the FFT decimation order), so within the
///   real range `0..N` it is guaranteed to hit every real leaf exactly once (we
///   simply SKIP padded indices `>= N`; the relative order of the survivors is a
///   permutation of `0..N`).
/// * SIMPLE: a single reverse-bits over `log2 P` bits. The exact heuristic
///   matters less than that it is deterministic, permutation-safe, and
///   front-loads the interleave — as the plan requires.
///
/// Real leaves keep their normal descriptor (`WorkDescriptor::leaf`); only the
/// *dispatch order* changes.
pub fn seed_leaf_descriptors_scheduled(
    radix: usize,
    leaf_count: usize,
    tx_per_proof: usize,
    order: SeedOrder,
) -> Vec<WorkDescriptor> {
    let now = now_epoch_ms();
    let indices: Vec<usize> = match order {
        SeedOrder::Sequential => (0..leaf_count).collect(),
        SeedOrder::CriticalPathFirst => critical_path_first_order(leaf_count),
    };
    indices
        .into_iter()
        .map(|i| {
            let mut d = WorkDescriptor::leaf(i, radix, leaf_count, tx_per_proof);
            d.dispatch_ts_ms = now;
            d
        })
        .collect()
}

/// (#321 Phase 8) Seed the N REDUCTION leaf descriptors in the requested
/// [`SeedOrder`], each STAMPED with `dispatch_ts_ms = now` at seed time. This is
/// the reduction-path analogue of [`seed_leaf_descriptors_scheduled`]: it reuses
/// the EXACT same deterministic ordering (`Sequential` / `CriticalPathFirst`
/// bit-reversal permutation of `0..N`) but builds each descriptor via
/// [`WorkDescriptor::reduction_leaf`] — a [`Role::Leaf`] tagged
/// [`FoldStrategy::Reduction`] with its single-leaf interval `[i, i]` at level 0.
///
/// Seeding these (instead of the hex [`WorkDescriptor::leaf`]) is the SWITCH that
/// engages the order-free reduction pipeline end-to-end on the fungible `work`
/// pool: the coordinator routes each leaf's completion to the interval gate by
/// the descriptor's `fold_strategy`, and the gate publishes the adjacent-pair
/// [`Role::ReductionFold`] tasks — no per-level `TreeNode` jobs are used.
///
/// Every returned `Vec` is a PERMUTATION of the leaf indices `0..N` (each leaf
/// exactly once); only the *sequence* differs by order.
pub fn seed_reduction_leaf_descriptors_scheduled(
    radix: usize,
    leaf_count: usize,
    tx_per_proof: usize,
    order: SeedOrder,
) -> Vec<WorkDescriptor> {
    let now = now_epoch_ms();
    let indices: Vec<usize> = match order {
        SeedOrder::Sequential => (0..leaf_count).collect(),
        SeedOrder::CriticalPathFirst => critical_path_first_order(leaf_count),
    };
    indices
        .into_iter()
        .map(|i| {
            let mut d = WorkDescriptor::reduction_leaf(i, radix, leaf_count, tx_per_proof);
            d.dispatch_ts_ms = now;
            d
        })
        .collect()
}

/// (#321 Phase 6) The deterministic `CriticalPathFirst` leaf-index sequence over
/// `N` real leaves: the bit-reversal permutation of the padded index space
/// `0..P` (`P = next_power_of_two(N)`), with padded indices `>= N` skipped. The
/// result is a PERMUTATION of `0..N` (every real leaf exactly once). See
/// [`seed_leaf_descriptors_scheduled`] for the rationale.
fn critical_path_first_order(leaf_count: usize) -> Vec<usize> {
    if leaf_count <= 1 {
        return (0..leaf_count).collect();
    }
    let padded = leaf_count.next_power_of_two();
    let bits = padded.trailing_zeros(); // log2(padded), exact for a power of two.
    let mut out = Vec::with_capacity(leaf_count);
    for i in 0..padded {
        let r = reverse_bits(i, bits);
        // Skip padded leaves: they carry no proof, so they are never seeded. The
        // survivors (real indices) form a permutation of 0..N.
        if r < leaf_count {
            out.push(r);
        }
    }
    out
}

/// (#321 Phase 6) Reverse the low `bits` bits of `value` — the primitive behind
/// the bit-reversal permutation used by [`critical_path_first_order`]. Pure and
/// deterministic.
fn reverse_bits(value: usize, bits: u32) -> usize {
    let mut v = value;
    let mut r = 0usize;
    for _ in 0..bits {
        r = (r << 1) | (v & 1);
        v >>= 1;
    }
    r
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
            fold_kind: "n/a".to_string(),
            merge_interval_span: 0,
            redriven_after_lease_expiry: false,
            pull_ts_ms: 0,
            scheduling_class: "sequential".to_string(),
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
        // #321 Phase 5 fields also default honestly on a pre-#328 payload.
        assert_eq!(e.fold_kind, "n/a", "missing fold_kind defaults to n/a");
        assert_eq!(e.merge_interval_span, 0);
        assert!(!e.redriven_after_lease_expiry);
    }

    /// (#321 Phase 5) A `ProverEvent` carrying the new reduction/recovery fields
    /// round-trips through JSON with every P5 field intact.
    #[test]
    fn prover_event_with_p5_fields_round_trips_through_json() {
        let event = ProverEvent {
            descriptor: WorkDescriptor::reduction_fold(0, 3, 2, 2, 4, 1),
            status: "success".to_string(),
            prove_time_ms: 10,
            gcs_time_ms: 2,
            total_time_ms: 15,
            peak_rss_bytes: 1024,
            prestate_source: "n/a".to_string(),
            pull_ms: 1,
            pre_exec_ms: 0,
            prove_ms: 10,
            gcs_write_ms: 2,
            queue_wait_ms: 0,
            is_first_task_on_pod: false,
            chunk_size: 1,
            leaf_count: 4,
            fold_kind: "real".to_string(),
            merge_interval_span: 4,
            redriven_after_lease_expiry: true,
            pull_ts_ms: 0,
            scheduling_class: "sequential".to_string(),
        };
        let s = serde_json::to_string(&event).unwrap();
        let back: ProverEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back.fold_kind, "real");
        assert_eq!(back.merge_interval_span, 4);
        assert!(back.redriven_after_lease_expiry);
        assert_eq!(back.descriptor.fold_strategy, FoldStrategy::Reduction);
    }

    /// (#321 Phase 5) A pre-Phase-5 `ProverEvent` JSON (WITHOUT the P5 fields)
    /// still deserializes — the new fields default to their honest sentinels.
    #[test]
    fn pre_phase5_event_without_p5_fields_still_deserializes() {
        // Note: descriptor also omits fold_strategy/redriven (pre-P5), so those
        // default too (Hex / false).
        let legacy = r#"{
            "descriptor":{
                "role":"leaf","radix":2,"leaf_count":8,"tx_per_proof":300,
                "chunk_idx":3,"level":0,"node_idx":0,"lo":0,"hi":0
            },
            "status":"success",
            "prove_time_ms":1,"gcs_time_ms":1,"total_time_ms":1,
            "peak_rss_bytes":1,"prestate_source":"corpus",
            "pull_ms":0,"pre_exec_ms":0,"prove_ms":1,"gcs_write_ms":1,
            "queue_wait_ms":0,"is_first_task_on_pod":false,
            "chunk_size":300,"leaf_count":8
        }"#;
        let e: ProverEvent =
            serde_json::from_str(legacy).expect("pre-P5 event must deserialize");
        assert_eq!(e.fold_kind, "n/a", "missing fold_kind defaults to n/a");
        assert_eq!(e.merge_interval_span, 0);
        assert!(!e.redriven_after_lease_expiry);
        assert_eq!(
            e.descriptor.fold_strategy,
            FoldStrategy::Hex,
            "missing fold_strategy defaults to Hex"
        );
        assert!(!e.descriptor.redriven);
    }

    /// (#321 Phase 5) `fold_strategy` serde round-trip + wire back-compat: a
    /// pre-Phase-5 `WorkDescriptor` JSON (no `fold_strategy`) deserializes to
    /// [`FoldStrategy::Hex`]; a reduction descriptor round-trips with
    /// [`FoldStrategy::Reduction`] intact (serialized kebab-case).
    #[test]
    fn fold_strategy_serde_round_trip_and_back_compat() {
        // Round-trip a reduction descriptor: fold_strategy survives.
        let d = WorkDescriptor::reduction_fold(0, 3, 2, 2, 4, 1);
        let s = serde_json::to_string(&d).unwrap();
        assert!(
            s.contains("\"reduction\""),
            "reduction fold_strategy must serialize kebab-case, got: {s}"
        );
        let back: WorkDescriptor = serde_json::from_str(&s).unwrap();
        assert_eq!(back.fold_strategy, FoldStrategy::Reduction);

        // A pre-Phase-5 descriptor (no fold_strategy / redriven) defaults to Hex.
        let legacy = r#"{
            "role":"tree-node","radix":2,"leaf_count":4,"tx_per_proof":1,
            "chunk_idx":0,"level":1,"node_idx":0,"lo":0,"hi":0
        }"#;
        let d2: WorkDescriptor =
            serde_json::from_str(legacy).expect("legacy descriptor must deserialize");
        assert_eq!(
            d2.fold_strategy,
            FoldStrategy::Hex,
            "missing fold_strategy defaults to Hex"
        );
        assert!(!d2.redriven, "missing redriven defaults to false");

        // FoldStrategy::default() is Hex.
        assert_eq!(FoldStrategy::default(), FoldStrategy::Hex);
    }

    /// (#321 Phase 5) The `reduction_leaf` ctor makes a `Role::Leaf` descriptor
    /// tagged `FoldStrategy::Reduction` with the single-leaf interval
    /// `[chunk_idx, chunk_idx]` at level 0 — the base of the reduction tree.
    #[test]
    fn reduction_leaf_ctor_sets_reduction_interval_and_level_zero() {
        let rl = WorkDescriptor::reduction_leaf(5, 2, 8, 1);
        assert_eq!(rl.role, Role::Leaf);
        assert_eq!(rl.fold_strategy, FoldStrategy::Reduction);
        assert_eq!(rl.lo, 5, "reduction leaf lo == chunk_idx");
        assert_eq!(rl.hi, 5, "reduction leaf hi == chunk_idx");
        assert_eq!(rl.chunk_idx, 5);
        assert_eq!(rl.level, 0, "reduction leaf is at level 0 (tree base)");
        assert!(!rl.redriven);
        // Its output key is still the leaf key (a reduction leaf proves a chunk).
        assert_eq!(rl.output_key(), "leaf_5.proof");

        // A plain (hex) leaf remains Hex with a zeroed interval — unchanged.
        let hl = WorkDescriptor::leaf(5, 2, 8, 1);
        assert_eq!(hl.fold_strategy, FoldStrategy::Hex);
        assert_eq!(hl.lo, 0);
        assert_eq!(hl.hi, 0);
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

    // ── #321 Phase 6: dispatch/pull timestamps + straggler-aware seeding ──────

    /// (#321 Phase 6) `dispatch_ts_ms` round-trips through JSON.
    #[test]
    fn dispatch_ts_ms_round_trips_through_json() {
        let mut d = WorkDescriptor::leaf(2, 2, 4, 1);
        d.dispatch_ts_ms = 1_700_000_000_123;
        let s = serde_json::to_string(&d).unwrap();
        let back: WorkDescriptor = serde_json::from_str(&s).unwrap();
        assert_eq!(back.dispatch_ts_ms, 1_700_000_000_123);
        assert_eq!(d, back);
    }

    /// (#321 Phase 6) A descriptor serialized BEFORE `dispatch_ts_ms` existed
    /// (no field) must still deserialize — `#[serde(default)]` supplies the honest
    /// `0` sentinel. Wire back-compat, mirroring the Phase-3 interval-field test.
    #[test]
    fn pre_phase6_descriptor_without_dispatch_ts_still_deserializes() {
        let legacy = r#"{
            "role":"leaf","radix":2,"leaf_count":4,"tx_per_proof":1,
            "chunk_idx":2,"level":0,"node_idx":0,"lo":0,"hi":0
        }"#;
        let d: WorkDescriptor =
            serde_json::from_str(legacy).expect("legacy descriptor must deserialize");
        assert_eq!(
            d.dispatch_ts_ms, 0,
            "missing dispatch_ts_ms must default to the honest 0 sentinel"
        );
        assert_eq!(d.chunk_idx, 2);
        assert_eq!(d.role, Role::Leaf);
    }

    /// (#321 Phase 6) A pure constructor emits an UNSTAMPED (0) descriptor — the
    /// gating engine's descriptors are pure/unit-testable — while `stamped_now`
    /// stamps a real (non-zero on this host) dispatch time at the publish boundary.
    #[test]
    fn constructors_are_unstamped_and_stamped_now_stamps() {
        let pure = WorkDescriptor::tree_node(1, 0, 2, 4, 1);
        assert_eq!(
            pure.dispatch_ts_ms, 0,
            "pure constructor must be unstamped (0) — keeps the engine testable"
        );
        let stamped = WorkDescriptor::leaf(0, 2, 4, 1).stamped_now();
        assert!(
            stamped.dispatch_ts_ms > 0,
            "stamped_now must record a real epoch-millis on this host"
        );
    }

    /// (#321 Phase 6) The HONEST queue_wait computation: with a stamped
    /// `dispatch_ts_ms = T` and a pull at `T + delta`, the wait is exactly delta.
    #[test]
    fn queue_wait_ms_is_pull_minus_dispatch_when_stamped() {
        let mut d = WorkDescriptor::leaf(0, 2, 4, 1);
        let t = 1_700_000_000_000u64;
        d.dispatch_ts_ms = t;
        // Pulled 250 ms after dispatch ⇒ queue_wait == 250.
        assert_eq!(d.queue_wait_ms_from_pull(t + 250), 250);
        // Pulled at the exact dispatch instant ⇒ 0.
        assert_eq!(d.queue_wait_ms_from_pull(t), 0);
    }

    /// (#321 Phase 6) An UNSTAMPED descriptor (`dispatch_ts_ms == 0`, the honest
    /// sentinel) yields a queue_wait of 0 — NEVER fabricated from a bogus zero
    /// baseline — regardless of the pull time.
    #[test]
    fn queue_wait_ms_stays_zero_when_unstamped() {
        let d = WorkDescriptor::leaf(0, 2, 4, 1); // dispatch_ts_ms == 0
        assert_eq!(d.dispatch_ts_ms, 0);
        assert_eq!(
            d.queue_wait_ms_from_pull(1_700_000_000_999),
            0,
            "unstamped => honest 0, never a huge fabricated wait"
        );
    }

    /// (#321 Phase 6) A clock skew that puts the pull time BEFORE the dispatch
    /// time saturates to 0 (never a wrapped huge number).
    #[test]
    fn queue_wait_ms_saturates_on_clock_skew() {
        let mut d = WorkDescriptor::leaf(0, 2, 4, 1);
        d.dispatch_ts_ms = 1_000;
        assert_eq!(d.queue_wait_ms_from_pull(500), 0);
    }

    /// (#321 Phase 6) `Sequential` seeding returns leaves in plain 0..N order,
    /// each STAMPED at seed time (dispatch_ts_ms > 0 on this host).
    #[test]
    fn seed_order_sequential_is_zero_to_n_stamped() {
        let seeds = seed_leaf_descriptors_scheduled(2, 6, 1, SeedOrder::Sequential);
        let order: Vec<usize> = seeds.iter().map(|d| d.chunk_idx).collect();
        assert_eq!(order, vec![0, 1, 2, 3, 4, 5]);
        assert!(
            seeds.iter().all(|d| d.dispatch_ts_ms > 0),
            "seeded leaves must be stamped at seed time"
        );
    }

    /// (#321 Phase 6) `CriticalPathFirst` seeding returns a PERMUTATION of 0..N
    /// (every leaf exactly once) and is DETERMINISTIC (identical across calls).
    /// Covers a power-of-two N and a non-power-of-two N (padding skipped).
    #[test]
    fn seed_order_critical_path_first_is_a_deterministic_permutation() {
        for &n in &[1usize, 2, 3, 4, 5, 8, 13, 16, 125] {
            let a = seed_leaf_descriptors_scheduled(2, n, 1, SeedOrder::CriticalPathFirst);
            let b = seed_leaf_descriptors_scheduled(2, n, 1, SeedOrder::CriticalPathFirst);
            assert_eq!(a.len(), n, "N={n}: must cover exactly N leaves");
            let idx_a: Vec<usize> = a.iter().map(|d| d.chunk_idx).collect();
            let idx_b: Vec<usize> = b.iter().map(|d| d.chunk_idx).collect();
            assert_eq!(idx_a, idx_b, "N={n}: must be deterministic");
            // A permutation of 0..N: every leaf exactly once.
            let mut sorted = idx_a.clone();
            sorted.sort_unstable();
            assert_eq!(
                sorted,
                (0..n).collect::<Vec<_>>(),
                "N={n}: must be a permutation of 0..N (each leaf exactly once)"
            );
        }
    }

    /// (#321 Phase 6) The `CriticalPathFirst` heuristic FRONT-LOADS the two
    /// top-level halves: for a power-of-two N the second seeded leaf is `N/2`
    /// (bit reversal interleaves the halves), so folding of BOTH top subtrees can
    /// begin from the first two leaves rather than after one whole half.
    #[test]
    fn critical_path_first_front_loads_top_level_halves() {
        let order: Vec<usize> = seed_leaf_descriptors_scheduled(2, 8, 1, SeedOrder::CriticalPathFirst)
            .iter()
            .map(|d| d.chunk_idx)
            .collect();
        // bitrev over 3 bits: 0,4,2,6,1,5,3,7.
        assert_eq!(order, vec![0, 4, 2, 6, 1, 5, 3, 7]);
        assert_eq!(order[0], 0, "first leaf is the left-most (subtree 0)");
        assert_eq!(order[1], 4, "second leaf is N/2 (the OTHER top-level half)");
    }

    /// (#321 Phase 8) The reduction seeder emits `FoldStrategy::Reduction`
    /// `Role::Leaf` descriptors with `lo == hi == chunk_idx` at level 0, in plain
    /// `0..N` order for `Sequential`, each STAMPED at seed time. This is the
    /// switch that engages the order-free reduction pipeline on the `work` pool.
    #[test]
    fn reduction_seeder_emits_reduction_leaves_sequential() {
        let seeds =
            seed_reduction_leaf_descriptors_scheduled(2, 6, 1, SeedOrder::Sequential);
        assert_eq!(seeds.len(), 6, "must cover exactly N reduction leaves");
        let order: Vec<usize> = seeds.iter().map(|d| d.chunk_idx).collect();
        assert_eq!(order, vec![0, 1, 2, 3, 4, 5], "sequential = 0..N");
        for (i, d) in seeds.iter().enumerate() {
            assert_eq!(d.role, Role::Leaf, "reduction leaf keeps Role::Leaf");
            assert_eq!(
                d.fold_strategy,
                FoldStrategy::Reduction,
                "reduction leaf must be tagged FoldStrategy::Reduction"
            );
            assert_eq!(d.lo, i, "reduction leaf interval lo == chunk index");
            assert_eq!(d.hi, i, "reduction leaf interval hi == chunk index");
            assert_eq!(d.level, 0, "reduction leaf sits at level 0 (tree base)");
            assert!(d.dispatch_ts_ms > 0, "seeded leaves must be stamped");
        }
    }

    /// (#321 Phase 8) The reduction seeder covers every leaf `0..N` exactly once
    /// under BOTH seed orders (it reuses the SAME ordering helper as the hex
    /// seeder): a permutation of `0..N`, deterministic, all tagged Reduction.
    #[test]
    fn reduction_seeder_covers_permutation_of_0_to_n() {
        for &n in &[1usize, 2, 3, 5, 8, 16, 125] {
            for order in [SeedOrder::Sequential, SeedOrder::CriticalPathFirst] {
                let seeds =
                    seed_reduction_leaf_descriptors_scheduled(2, n, 1, order);
                assert_eq!(seeds.len(), n, "N={n}: covers exactly N leaves");
                let mut idx: Vec<usize> = seeds.iter().map(|d| d.chunk_idx).collect();
                idx.sort_unstable();
                assert_eq!(
                    idx,
                    (0..n).collect::<Vec<_>>(),
                    "N={n}: reduction seeds are a permutation of 0..N"
                );
                assert!(
                    seeds
                        .iter()
                        .all(|d| d.fold_strategy == FoldStrategy::Reduction
                            && d.lo == d.chunk_idx
                            && d.hi == d.chunk_idx),
                    "N={n}: every reduction leaf tagged Reduction with [i, i]"
                );
            }
        }
    }

    /// (#321 Phase 8) The reduction seeder reuses the EXACT `CriticalPathFirst`
    /// ordering of the hex seeder — only the descriptor tagging differs. This is
    /// the invariant that keeps the straggler-aware scheduling identical across
    /// strategies.
    #[test]
    fn reduction_seeder_reuses_hex_seed_order() {
        let hex: Vec<usize> =
            seed_leaf_descriptors_scheduled(2, 8, 1, SeedOrder::CriticalPathFirst)
                .iter()
                .map(|d| d.chunk_idx)
                .collect();
        let reduction: Vec<usize> =
            seed_reduction_leaf_descriptors_scheduled(2, 8, 1, SeedOrder::CriticalPathFirst)
                .iter()
                .map(|d| d.chunk_idx)
                .collect();
        assert_eq!(hex, reduction, "reduction reuses the hex seed order exactly");
    }

    /// (#321 Phase 8) The SERDE WIRE default stays `Hex` for descriptor
    /// back-compat: a `WorkDescriptor` serialized BEFORE the `fold_strategy`
    /// field existed deserializes to `Hex`. This is DISTINCT from the CLI default
    /// (flipped to Reduction in prover_node.rs) — the wire default must NOT change.
    #[test]
    fn wire_fold_strategy_default_stays_hex() {
        assert_eq!(FoldStrategy::default(), FoldStrategy::Hex);
        // A JSON descriptor missing the field deserializes to Hex.
        let json = r#"{"role":"leaf","radix":2,"leaf_count":4,"tx_per_proof":1,
            "chunk_idx":0,"level":0,"node_idx":0}"#;
        let d: WorkDescriptor = serde_json::from_str(json).unwrap();
        assert_eq!(
            d.fold_strategy,
            FoldStrategy::Hex,
            "pre-#321 descriptor (no field) must deserialize to Hex (wire back-compat)"
        );
    }

    /// (#321 Phase 6) `SeedOrder::as_str` emits the stable wire strings echoed
    /// into `ProverEvent::scheduling_class`.
    #[test]
    fn seed_order_wire_strings_are_stable() {
        assert_eq!(SeedOrder::Sequential.as_str(), "sequential");
        assert_eq!(SeedOrder::CriticalPathFirst.as_str(), "critical-path-first");
        assert_eq!(SeedOrder::default(), SeedOrder::Sequential);
    }

    /// (#321 Phase 6) A `ProverEvent` carrying the new `pull_ts_ms` +
    /// `scheduling_class` fields round-trips through JSON with both intact.
    #[test]
    fn prover_event_with_p6_fields_round_trips_through_json() {
        let event = ProverEvent {
            descriptor: WorkDescriptor::leaf(1, 2, 8, 1),
            status: "success".to_string(),
            prove_time_ms: 5,
            gcs_time_ms: 1,
            total_time_ms: 7,
            peak_rss_bytes: 0,
            prestate_source: "corpus".to_string(),
            pull_ms: 2,
            pre_exec_ms: 0,
            prove_ms: 5,
            gcs_write_ms: 1,
            queue_wait_ms: 42,
            is_first_task_on_pod: false,
            chunk_size: 1,
            leaf_count: 8,
            fold_kind: "n/a".to_string(),
            merge_interval_span: 0,
            redriven_after_lease_expiry: false,
            pull_ts_ms: 1_700_000_000_500,
            scheduling_class: "critical-path-first".to_string(),
        };
        let s = serde_json::to_string(&event).unwrap();
        let back: ProverEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back.pull_ts_ms, 1_700_000_000_500);
        assert_eq!(back.scheduling_class, "critical-path-first");
        assert_eq!(back.queue_wait_ms, 42);
    }

    /// (#321 Phase 6) A pre-Phase-6 `ProverEvent` JSON (no `pull_ts_ms` /
    /// `scheduling_class`) still deserializes: `pull_ts_ms` defaults to the honest
    /// `0` and `scheduling_class` to the historical `"sequential"`. Wire
    /// back-compat, mirroring the pre-#328 event test.
    #[test]
    fn pre_phase6_event_without_scheduling_fields_still_deserializes() {
        let legacy = r#"{
            "descriptor":{
                "role":"leaf","radix":2,"leaf_count":8,"tx_per_proof":1,
                "chunk_idx":1,"level":0,"node_idx":0,"lo":0,"hi":0
            },
            "status":"success",
            "prove_time_ms":5,"gcs_time_ms":1,"total_time_ms":7
        }"#;
        let e: ProverEvent =
            serde_json::from_str(legacy).expect("legacy event must deserialize");
        assert_eq!(e.pull_ts_ms, 0, "missing pull_ts_ms defaults to honest 0");
        assert_eq!(
            e.scheduling_class, "sequential",
            "missing scheduling_class defaults to the historical sequential"
        );
        // And the descriptor's dispatch_ts_ms also defaults to 0.
        assert_eq!(e.descriptor.dispatch_ts_ms, 0);
    }
}
