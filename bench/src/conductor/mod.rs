// Copyright (c) Elliot Technologies, Inc.
// SPDX-License-Identifier: BUSL-1.1

//! The MINIMUM distributed-prover conductor (issue #75, local slice).
//!
//! This module implements the smallest operational distribution layer that
//! can dispatch block/chunk proving work across a (locally-simulated) fleet
//! of coordinators+cells AND account for witness-fetch cost, tested entirely
//! on the host with no GCP.
//!
//! ## Normative design (do NOT redesign here)
//!
//! - **ADR-0006** (`docs/decisions/ADR-0006-distributed-prover-conductor.md`)
//!   — the conductor: two-tier dispatch (§1: OUTER block-dispatch +
//!   INNER chunk fan-out), the coordinator POOL (§2, horizontal scaling,
//!   #113), and the witness-plane **seam** (§3, #61).
//! - **ADR-0008** (`docs/decisions/ADR-0008-witness-delivery-plane.md`)
//!   — the witness *delivery mechanism* behind that seam: `{height,
//!   witness_index}` addressing (§1.1), references-not-bytes dispatch
//!   (§1.2), the k=1 `bench_test.json` degenerate case (§1.4), and the
//!   exact `witness_fetch_ms` instrumentation point (§2).
//! - **ADR-0004** (`docs/decisions/ADR-0004-unified-recursive-distribution.md`)
//!   — the model the conductor executes: the governing equation and the
//!   `lag(c, l)` decomposition (§3.1/§3.2), with `witness_move` named as
//!   the one UNMODELED term measured via #61.
//!
//! ## What this local slice IS
//!
//! - **OUTER dispatch** ([`queue`]): a `BlockQueue` trait (the ADR-0006 §1.1
//!   competing-pull block-dispatch layer) with a local in-memory adapter so
//!   it runs without GCP. A real Pub/Sub adapter drops in behind the same
//!   trait later (the trait is the seam ADR-0006 §1.1 names).
//! - **INNER dispatch** ([`dispatch`]): a coordinator that SPLITs its block
//!   into `k` chunks and fans them out to a **horizontal pool** of cells
//!   (ADR-0006 §1.2 + §2; #113 PRIMARY lever only), reusing the
//!   `bench::stream` bounded-queue + closure-injection pattern so the
//!   prover is INJECTED and the whole path is testable without plonky2.
//! - **WITNESS delivery** ([`witness`]): `{height, witness_index}` addressing,
//!   a k=1 LOCAL mounted-corpus resolver, and the real `witness_fetch_ms`
//!   measurement seam (ADR-0008 §1.1/§1.4/§2.1).
//!
//! ## Cloud transport (issue #172 — the real Pub/Sub drop-in)
//!
//! - **REAL Pub/Sub** ([`pubsub`]): the cloud drop-in the bullet below once
//!   named as "future" now EXISTS — [`pubsub::GcloudPubSub`] implements the
//!   same [`queue::BlockQueue`] trait over real Pub/Sub (via the `gcloud` CLI
//!   already in the runtime image), and adds the chunk-dispatch + results
//!   planes the live coordinator/cell pods use. The in-memory
//!   [`queue::LocalBlockQueue`] stays for host tests. See
//!   `docs/distributed-prover-runtime.md` and `bench --mode coordinator|cell`.
//!
//! ## What this conductor slice is NOT (still out of scope)
//!
//! - **No MIG, no provisioning here.** The GKE topology + Pub/Sub resources
//!   live in `cicd/terraform/gke/`. This crate only speaks the protocol.
//! - **No per-coordinator vertical concurrency** (#113 SECONDARY lever —
//!   deferred). The pool is HORIZONTAL only.
//! - **No matching engine** (#125, closed).
//! - **`witness_fetch_ms` is the LOCAL-RESOLVE FLOOR, never `witness_move`.**
//!   ADR-0008 §2.3 is explicit: the local read is the floor; the distributed
//!   `witness_move` term stays UNMODELED until ADR-0008 §3's gated study runs
//!   on a fleet over varied (G2) witnesses. No fetch-cost number is invented.

pub mod dispatch;
pub mod pubsub;
pub mod queue;
pub mod witness;

pub use dispatch::{Coordinator, CoordinatorPool, InnerDispatchOutcome};
pub use pubsub::{
    BlockMessage, ChunkMessage, ChunkResultMessage, GcloudPubSub, PubSubConfig, PulledMessage,
};
pub use queue::{BlockJob, BlockQueue, LocalBlockQueue};
pub use witness::{MountedCorpus, ResolvedWitness, WitnessKey, WitnessResolver, WitnessSlice};
